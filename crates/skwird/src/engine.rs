//! The engine's writer: one thread owns each root's index and its log
//! (`skwird_index::keeper`), and is the only one to change them. Queries
//! read the snapshot it publishes, swapped whole: one writer, so no write
//! conflicts to resolve; queries hold no lock while they run.
//!
//! It opens each root (its index read back, or crawled), then waits for a
//! command: a sync (the check, run now; syncs asked meanwhile served by
//! the same refresh), the journal, a client's registrations. Nothing wakes
//! it otherwise: idle is idle.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use skwird_crawl::fanotify::{self, Group, Resolved};
use skwird_crawl::mounts::Table;
use skwird_crawl::{Cancel, Exclusions, Heard, Options, Root, Time, Watcher};
use skwird_index::Index;
use skwird_index::block::FolderId;
use skwird_index::keeper::{Complete, Event, Keeper, Opened, Reconciled, TooOld};
use skwird_index::log::Token;
use skwird_index::tree::Limits;
use skwird_protocol::WirePath;

use crate::dirs::Dirs;
use crate::live::{Gather, HeardFolder};

/// How an engine runs.
#[derive(Clone, Debug)]
pub struct Config {
    pub dirs: Dirs,
    /// The roots it covers, as the system resolves them.
    pub roots: Vec<PathBuf>,
    /// How long it stays once its last client left: none for always.
    pub idle: Option<std::time::Duration>,
    pub threads: usize,
    /// The most folders it watches: none for the user's share.
    pub watches: Option<usize>,
    /// The system service (phase 3): every user's files, whole
    /// filesystems marked (fanotify), its own folders the system's.
    pub system: bool,
    /// Its roots found in the mount table, not named: followed as
    /// filesystems are mounted and unmounted.
    pub discovered: bool,
}

/// What a root publishes for queries to read.
#[derive(Clone)]
pub enum Published {
    /// Being opened: read back, or crawled.
    Opening,
    Ready(Arc<Index>),
    /// It could not be opened (why, without a path).
    Failed(String),
}

/// What the writer and the connections share.
pub struct Shared {
    /// Each root's path, and whether it is there: at the system service, a
    /// filesystem mounted since the start is a root too, and one unmounted
    /// is one no longer. A root keeps its number for the engine's life.
    roots: RwLock<Vec<(PathBuf, bool)>>,
    /// Whether this is the system service.
    pub system: bool,
    /// For the system service, the folders above each root, outermost
    /// first, as they were when it became one: for the folders no root
    /// holds, which an asker must search too (DESIGN 4).
    above: RwLock<Vec<Vec<Option<crate::asker::Owned>>>>,
    /// The queries each user runs now (`serve::Running`).
    pub running: Arc<std::sync::Mutex<std::collections::HashMap<u32, usize>>>,
    /// The connections each user holds open now.
    pub connections: Arc<std::sync::Mutex<std::collections::HashMap<u32, usize>>>,
    /// When each user last asked the writer for work (a sync, a
    /// registration, the journal), at the system service.
    pub asked: std::sync::Mutex<std::collections::HashMap<u32, Instant>>,
    pub published: RwLock<Vec<Published>>,
    /// What the writer tells the readers of what it hears.
    pub readers: crate::live::Readers,
}

/// The folders above a root, outermost first, as they are now, for the
/// system service (none for a user's engine).
fn above_of(root: &Path, system: bool) -> Vec<Option<crate::asker::Owned>> {
    let mut above: Vec<_> = root
        .ancestors()
        .skip(1)
        .filter(|_| system)
        .map(owned_at)
        .collect();
    above.reverse();
    above
}

impl Shared {
    pub fn new(roots: Vec<PathBuf>, system: bool) -> Shared {
        let n = roots.len();
        let above = roots.iter().map(|root| above_of(root, system)).collect();
        Shared {
            roots: RwLock::new(roots.into_iter().map(|r| (r, true)).collect()),
            system,
            above: RwLock::new(above),
            running: Arc::default(),
            connections: Arc::default(),
            asked: std::sync::Mutex::default(),
            published: RwLock::new(vec![Published::Opening; n]),
            readers: crate::live::Readers::default(),
        }
    }

    fn read_roots(&self) -> std::sync::RwLockReadGuard<'_, Vec<(PathBuf, bool)>> {
        self.roots
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Root `i`'s path.
    pub fn root(&self, i: usize) -> PathBuf {
        self.read_roots()
            .get(i)
            .map(|(p, _)| p.clone())
            .unwrap_or_default()
    }

    /// The roots there now.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.read_roots()
            .iter()
            .filter(|(_, there)| *there)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// The folders above root `i`, outermost first, as they were when it
    /// became one.
    pub fn above(&self, i: usize) -> Vec<Option<crate::asker::Owned>> {
        self.above
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(i)
            .cloned()
            .unwrap_or_default()
    }

    /// `path` a root from now on, opening: its number, the one it had if
    /// it was one before.
    pub fn add_root(&self, path: &Path) -> usize {
        let mut roots = self
            .roots
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut published = self
            .published
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut above = self
            .above
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = above_of(path, self.system);
        if let Some(i) = roots.iter().position(|(p, _)| p == path) {
            roots[i].1 = true;
            published[i] = Published::Opening;
            above[i] = fresh;
            return i;
        }
        roots.push((path.to_path_buf(), true));
        published.push(Published::Opening);
        above.push(fresh);
        roots.len() - 1
    }

    /// Root `i` no longer one: its filesystem unmounted. A path there is
    /// answered by the root holding it now.
    pub fn drop_root(&self, i: usize) {
        if let Some(r) = self
            .roots
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(i)
        {
            r.1 = false;
        }
        self.publish(i, Published::Failed("no longer mounted".into()));
    }

    /// Whether root `i` is still a root: not let go of since.
    pub fn is_alive(&self, i: usize) -> bool {
        self.read_roots().get(i).is_some_and(|r| r.1)
    }

    /// The other roots strictly below `path`, which root `of` holds: the
    /// system service's other filesystems mounted below the folder asked.
    pub fn roots_below(&self, path: &Path, of: usize) -> Vec<usize> {
        self.read_roots()
            .iter()
            .enumerate()
            .filter(|&(j, (r, there))| {
                *there && j != of && r.starts_with(path) && r.as_path() != path
            })
            .map(|(j, _)| j)
            .collect()
    }

    /// The root holding `path`, by its spelling, and the path below it.
    pub fn locate(&self, path: &Path) -> Option<(usize, PathBuf)> {
        self.read_roots()
            .iter()
            .enumerate()
            .filter(|(_, (r, there))| *there && path.starts_with(r))
            .max_by_key(|(_, (r, _))| r.components().count())
            .map(|(i, (r, _))| {
                (
                    i,
                    path.strip_prefix(r).unwrap_or(Path::new("")).to_path_buf(),
                )
            })
    }

    pub fn snapshot(&self, root: usize) -> Published {
        self.published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[root]
            .clone()
    }

    fn publish(&self, root: usize, p: Published) {
        self.published
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[root] = p;
    }
}

/// What a journal's request is answered: its events, the token to ask
/// from next, and how far the events are every change; or too old.
pub(crate) type JournalAnswer = Result<(Vec<Event>, Token, Complete), TooOld>;

pub enum Command {
    /// Prove the root current now; answered once its snapshot is
    /// published.
    Sync {
        root: usize,
        /// The path asked, below the root: live, a barrier serves it.
        rel: PathBuf,
        reply: Sender<()>,
    },
    Changes {
        root: usize,
        rel: PathBuf,
        since: String,
        reply: Sender<JournalAnswer>,
    },
    /// A client's registrations: folders it keeps out of the index.
    Exclude {
        client: String,
        /// At the system service, the user asking.
        uid: Option<u32>,
        add: Vec<PathBuf>,
        remove: Vec<PathBuf>,
        reply: Sender<Vec<PathBuf>>,
    },
    /// A client's folders to keep live.
    Watch {
        client: String,
        add: Vec<PathBuf>,
        remove: Vec<PathBuf>,
        reply: Sender<skwird_protocol::Watching>,
    },
    /// A client's removable volumes to keep, forget and release, by
    /// identity or by mount point (`crate::volumes`): answered what it
    /// keeps after, and what is mounted, with the reads of those let go of
    /// still to end; or why not.
    Volumes {
        client: String,
        keep: Vec<String>,
        forget: Vec<String>,
        release: Vec<String>,
        release_at: Vec<PathBuf>,
        reply: Sender<Result<Released, String>>,
    },
    /// The mount table changed ([`crate::live::listen`]); on Windows, a
    /// volume arrived or went (`crate::serve::volumes_changed`).
    Mounts,
    /// The removal of root `root`'s volume is asked (Windows' service
    /// manager): its handles let go of, then `reply` answered.
    #[cfg(windows)]
    LetGo {
        root: usize,
        reply: Sender<()>,
    },
    /// The eject of the kept volume `id` was refused, another program
    /// holding it (Windows): its release ended, it is a root again.
    #[cfg(windows)]
    EjectRefused {
        id: String,
    },
    /// What the watcher of this generation heard ([`crate::live::listen`]).
    Heard {
        generation: u64,
        heard: Vec<Heard>,
    },
    /// What the group of this generation heard ([`crate::live::listen`]).
    Marks {
        generation: u64,
        heard: Vec<fanotify::Heard>,
    },
    /// Root `root` opened on a thread of its own ([`Writing::open_later`]),
    /// or a kept volume's checked ([`Writing::check_later`]), the
    /// `generation`th opening the writer began: its keeper, none if it
    /// could not be, whether it was built, and whether it was proven
    /// current there (by its crawl, or by the check of an index read back).
    Opened {
        root: usize,
        generation: u64,
        keeper: Option<Box<Keeper>>,
        built: bool,
        current: bool,
    },
    Stop,
}

impl Command {
    /// Whether a client asked for it: its work is not what is heard, and
    /// is not charged to the writer's share (`live::DUTY`).
    fn asked(&self) -> bool {
        matches!(
            self,
            Command::Sync { .. }
                | Command::Changes { .. }
                | Command::Exclude { .. }
                | Command::Watch { .. }
                | Command::Volumes { .. }
        )
    }
}

/// A folder a client keeps out of the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Registration {
    client: String,
    /// At the system service, the user it is for: it applies only to
    /// folders they own (DESIGN 5). None at a user's engine, whose user
    /// it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    path: WirePath,
}

/// The files of what clients registered, in the state folder: folders
/// kept out of the index, and folders kept live.
const EXCLUDED: &str = "registered";
const WATCHED: &str = "watched";

/// A folder heard, found in root `root`'s index while it had the shape
/// `shape` ([`Keeper::holds`]).
#[derive(Clone, Copy)]
struct Found {
    root: usize,
    folder: FolderId,
    id: skwird_crawl::FileId,
    shape: u64,
}

/// Where the folder an event names was found ([`Writing::find_heard`]).
#[derive(Clone, Copy)]
enum Where {
    /// In this root's index, as this folder.
    At(usize, FolderId),
    /// In a root being opened: what is heard in it is held.
    Held,
    /// In none, or not found.
    Nowhere,
}

/// What finding a folder heard gave, but a folder of an index: kept while
/// it holds, so that a folder heard again is not found again for each batch
/// of its events (`Writing::find_heard`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unfound {
    /// In no index (outside every root, kept out, not read yet), or reached
    /// by no mount: while the process's shapes stay
    /// ([`skwird_index::tree::shapes`]), since a folder comes into an index,
    /// or is renamed in one, only by a change that moves them.
    Nowhere(u64),
    /// In this root, being opened: what is heard in it is held, while the
    /// root is still opening, and found again as it is replayed.
    Held(usize),
    /// Gone: its handle never opens again.
    Gone,
}

/// The most folders heard the system service keeps as found: past them,
/// every one is let go of, and found again when heard.
const FOUND_KEPT: usize = 4096;

/// The most folders a user keeps out at the system service, each derived
/// again at every sync.
const MOST_REGISTERED: usize = 256;

/// What clients registered, kept across restarts.
fn registered<T: serde::de::DeserializeOwned>(dirs: &Dirs, file: &str) -> Vec<T> {
    std::fs::read(dirs.state.join(file))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// A folder above a root as the system service judges it, where no root
/// holds it: on Unix by its stat.
#[cfg(unix)]
fn owned_at(path: &Path) -> Option<crate::asker::Owned> {
    crate::asker::owned_at(path)
}

/// On Windows, a folder there, judged by its descriptor
/// (`skwird_crawl::judged_by`): an asker is asked only to pass through
/// it, which a token that may bypass traverse checking does unchecked
/// (`SeChangeNotifyPrivilege`, every user's by default), as Windows lets
/// it; one that may not is judged by the folder's descriptor. Where it
/// cannot be read, its security id is none (0), and such a token is not
/// known to, and is answered nothing below (DESIGN 4, "On Windows"). None
/// where no folder is there.
#[cfg(windows)]
fn owned_at(path: &Path) -> Option<crate::asker::Owned> {
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let (dev, owner) = skwird_crawl::judged_by(path).unwrap_or((0, 0));
    Some(crate::asker::Owned {
        owner,
        group: 0,
        mode: 0,
        dev,
    })
}

fn save_registered<T: Serialize>(dirs: &Dirs, file: &str, all: &[T]) {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let path = dirs.state.join(file);
    let tmp = dirs.state.join(format!("{file}.tmp"));
    // Its user's alone (0600), whatever the umask: the system service takes
    // a user's file only if no one else may write it.
    let write = |bytes: &[u8]| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // On Windows, the state folder's access, its profile's.
        #[cfg(unix)]
        options.mode(0o600);
        options
            .open(&tmp)
            .and_then(|mut f| f.write_all(bytes))
            .is_ok()
    };
    let ok = crate::dirs::make_private(&dirs.state).is_ok()
        && serde_json::to_vec(all).is_ok_and(|b| write(&b))
        && std::fs::rename(&tmp, &path).is_ok();
    if !ok {
        log("cannot keep what clients registered");
    }
}

/// The user's own exclusions: one absolute path a line, `~/` its home,
/// `#` a comment.
fn configured(dirs: &Dirs) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(dirs.config.join("exclude")) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.strip_prefix("~/") {
            Some(rest) => dirs.home.join(rest),
            None => PathBuf::from(l),
        })
        .collect()
}

/// Every exclusion: the defaults, the engine's own folders, the user's,
/// what clients registered; and how many were refused. The system
/// service's defaults hold in every home, and it leaves the Nix store out
/// (DESIGN 1, 5).
fn exclusions(config: &Config, registrations: &[Registration]) -> (Exclusions, usize) {
    let dirs = &config.dirs;
    // The system service's defaults are each user's, kept as theirs are
    // (`users_own`): resolved as the service, a link a user put there would
    // take another's folder out. The Nix store is Unix's: on Windows it is
    // no absolute path, and would be counted refused.
    let (homes, store) = if config.system {
        let store = cfg!(unix).then(|| PathBuf::from(crate::system::NIX_STORE));
        (Vec::new(), store.into_iter().collect())
    } else {
        (vec![dirs.home.clone()], Vec::new())
    };
    // The engine's own folders too: its index, its log, its state and its
    // cookies. Indexed, and watched, the engine would hear its own saves,
    // and read itself again without end.
    let own = [dirs.cache.clone(), dirs.state.clone(), dirs.runtime.clone()];
    let paths = homes
        .iter()
        .flat_map(|home| skwird_crawl::exclude::defaults(home))
        .chain(configured(dirs))
        .chain(
            registrations
                .iter()
                .filter(|r| r.uid.is_none())
                .map(|r| r.path.to_path()),
        )
        .chain(own)
        .chain(store);
    let (mut exclusions, refused) = Exclusions::resolve(paths);
    let mut refused = refused.len();
    if config.system {
        refused += users_own(&mut exclusions, registrations);
    }
    (exclusions, refused)
}

/// Each user's own exclusions, for the system service (DESIGN 5): their
/// file, what their engine's clients registered, and what their clients
/// registered with the service, each kept only if they own the folder it
/// resolves to. Gives how many were refused, theirs or not resolvable.
fn users_own(exclusions: &mut Exclusions, registrations: &[Registration]) -> usize {
    let users = crate::system::users();
    let mut refused = 0;
    let mut files = 0;
    for (uid, home) in &users {
        // A default at its own place in the home, no link in its way, holds
        // whoever owns it; reached through a link, only if the user owns
        // what it leads to: resolved as the service, a link a user put
        // there would take another's folder out. Not counted: a system
        // user's home, owned by root, would refuse them all.
        let at_home = skwird_crawl::canonical(home).unwrap_or_else(|_| home.clone());
        for rel in skwird_crawl::exclude::DEFAULTS {
            let place = at_home.join(rel);
            let held = exclusions.insert_if(&place, |resolved, _| resolved == place);
            if !matches!(held, Ok(true)) {
                theirs_kept(exclusions, *uid, home, &home.join(rel));
            }
        }
        let (theirs, untaken) = crate::system::own_exclusions(home, *uid);
        files += untaken;
        for path in theirs {
            refused += usize::from(!theirs_kept(exclusions, *uid, home, &path));
        }
    }
    for r in registrations {
        let Some(uid) = r.uid else { continue };
        // A user the database does not name owns no home to keep a folder
        // not made yet in: theirs only where they own one there now.
        let home = users
            .iter()
            .find(|(u, _)| *u == uid)
            .map_or_else(PathBuf::new, |(_, h)| h.clone());
        refused += usize::from(!theirs_kept(exclusions, uid, &home, &r.path.to_path()));
    }
    if files > 0 {
        log(&format!(
            "{files} users' files of exclusions not taken: not a file of theirs, or \
             another user may write it"
        ));
    }
    refused
}

/// `path` excluded for user `uid`, of home `home`, if they own the folder
/// it resolves to ([`crate::system::owned`]): whether it was.
fn theirs_kept(exclusions: &mut Exclusions, uid: u32, home: &Path, path: &Path) -> bool {
    let home = skwird_crawl::canonical(home).unwrap_or_else(|_| home.to_path_buf());
    matches!(
        exclusions.insert_if(path, |resolved, id| crate::system::owned(
            uid, &home, resolved, id
        )),
        Ok(true)
    )
}

fn log_refused(refused: usize) {
    if refused > 0 {
        log(&format!(
            "{refused} exclusions refused: empty, relative, unresolvable, or a user's for a folder not theirs"
        ));
    }
}

/// What a root's index did to hold to its exclusions, if anything.
fn log_reconciled(root: usize, r: Reconciled) {
    if r.purged > 0 || r.readmitted > 0 {
        log(&format!(
            "root {root} held to its exclusions: {} folders purged, {} to read again",
            r.purged, r.readmitted
        ));
    }
    if r.unsaved {
        log("a purge's save failed: tried again at the next save");
    }
}

/// The syncs `replies` answered.
fn answered(replies: Vec<Sender<()>>) {
    for reply in replies {
        let _ = reply.send(());
    }
}

/// `wait` passed, or `cancel` asked sooner.
fn pause(wait: std::time::Duration, cancel: &Cancel) {
    let until = Instant::now() + wait;
    while Instant::now() < until && !cancel.is_cancelled() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Where a root's index is kept: a folder named by its path's hash, of
/// its bytes as the system names it (on Windows, its WTF-8, as the wire
/// carries it: std's own encoding is not promised, and a change of it
/// would rename every index).
pub fn index_dir(dirs: &Dirs, root: &Path) -> PathBuf {
    let hash = fnv1a(&skwird_protocol::WirePath::of(root).0);
    dirs.cache.join(format!("index-{hash:016x}"))
}

/// The 64-bit FNV-1a of `bytes`, which names the folders of the engine's
/// cache: never changed, or every index would be left behind.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// A line of the engine's log: counts, sizes and times, never a name or a
/// path, so that a user may share it.
pub fn log(line: &str) {
    eprintln!("skwird: {line}");
}

/// The writer's loop: open every root, then serve commands until told to
/// stop or every sender is gone. With a watcher, the folders clients
/// registered are watched, what it heard applied as it comes (DESIGN 8):
/// the writer then waits with a deadline, but only while something is
/// due.
/// The writer's turns, until it is stopped: each command answered, and
/// what is due done.
fn turns(w: &mut Writing<'_>, commands: &Receiver<Command>) {
    // Each turn's processor time is charged to the account (`live::DUTY`),
    // but what a client asked for: waking and the bookkeeping too.
    let mut mark = thread_time();
    loop {
        let command = match w.pending.pop() {
            Some(c) => Some(c),
            None => match w.deadline() {
                None => match commands.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                },
                Some(at) => {
                    match commands.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(c) => Some(c),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            },
        };
        w.done.wakes += 1;
        let mut asked = std::time::Duration::ZERO;
        if let Some(command) = command {
            let by_client = command.asked();
            let start = thread_time();
            // Asked to stop while roots are opening, it stops once they are,
            // so that a first build is kept.
            if !w.answer(commands, command) {
                // A kept volume's first read is not waited for: it stops.
                w.cancel_volumes();
                if w.opening.is_empty() {
                    break;
                }
                w.stopping = true;
            }
            if by_client {
                asked = thread_time().saturating_sub(start);
            }
        }
        // Heard without pause, the deadline would never pass.
        w.on_time();
        // Derived again from the syncs waiting, after every turn.
        w.shared.readers.set_awaited(!w.waiting.is_empty());
        let end = thread_time();
        asked += std::mem::take(&mut w.for_clients);
        w.gather.turned(
            end.saturating_sub(mark).saturating_sub(asked),
            Instant::now(),
        );
        mark = end;
        if w.stopping && w.opening.is_empty() {
            break;
        }
    }
}

pub fn writer(
    config: &Config,
    shared: &Arc<Shared>,
    commands: &Receiver<Command>,
    sender: Sender<Command>,
    watcher: Option<Arc<Watcher>>,
    group: Option<Arc<Group>>,
) {
    let opts = Options {
        // Each folder watched as it is read, where roots are not heard
        // whole (Linux); where they are, no folder is watched alone.
        watcher: watcher.clone().filter(|_| !HEARD_WHOLE),
        threads: config.threads,
        // The system service answers each asker by each folder's ACL too.
        acls: config.system,
        ..Options::default()
    };
    let registrations = registered(&config.dirs, EXCLUDED);
    let kept_live = registered(&config.dirs, WATCHED);
    // Wanted before the first crawl, so that a first build watches them
    // as it reads them.
    if let Some(w) = &watcher {
        w.set_scopes(resolved(&kept_live));
    }
    let cancel = Cancel::default();
    let table = Table::read().unwrap_or_default();
    // Each root's filesystem marked before it is read (DESIGN 2, "watch,
    // then read"): what changes from now on is heard.
    // On Windows, each root's tree watched whole before it is read, as a
    // filesystem is marked (phase 4, step 5).
    // The system service's roots resumed from the journal's position kept.
    #[cfg(any(windows, target_os = "macos"))]
    let mut resuming = Vec::new();
    let marked: Vec<Option<(u32, u32)>> = match (&group, &watcher) {
        (Some(g), _) => config
            .roots
            .iter()
            .enumerate()
            .map(|(i, root)| mark(g, i, root, &table))
            .collect(),
        // On macOS, each root heard whole in the roots' stream before it
        // is read (phase 5, step 3), and replayed from its position kept
        // (step 4).
        #[cfg(any(windows, target_os = "macos"))]
        (None, Some(w)) => config
            .roots
            .iter()
            .enumerate()
            .map(|(i, root)| {
                let (marked, resumed) = follow(config, w, i, root);
                if resumed {
                    resuming.push(i);
                }
                marked
            })
            .collect(),
        _ => vec![None; config.roots.len()],
    };
    let since = Time::now();
    let cookies = match (&watcher, &group) {
        (Some(w), _) => cookie_folder(&config.dirs, w),
        (None, Some(g)) => marked_cookie_folder(&config.dirs, g, &table),
        (None, None) => None,
    };
    let (ex, refused) = exclusions(config, &registrations);
    log_refused(refused);
    let mut w = Writing {
        config,
        shared,
        cancel,
        #[cfg(any(windows, target_os = "macos"))]
        journals: Journals::started(config.roots.len(), &resuming, since),
        opts: opts.clone(),
        keepers: (0..config.roots.len()).map(|_| None).collect(),
        registrations,
        kept_live,
        watcher,
        group,
        table,
        marked,
        found: HashMap::new(),
        unfound: HashMap::new(),
        gather: Gather::default(),
        cookies,
        next_cookie: 0,
        waiting: Vec::new(),
        volumes: crate::volumes::State::kept(if config.system {
            Vec::new()
        } else {
            registered(&config.dirs, crate::volumes::FILE)
        }),
        volume_roots: HashMap::new(),
        served: Vec::new(),
        for_clients: std::time::Duration::ZERO,
        done: Done {
            since: thread_time(),
            ..Done::default()
        },
        pending: Vec::new(),
        opening: HashMap::new(),
        held: Vec::new(),
        parked: Vec::new(),
        stopping: false,
        registered: 0,
        openings: 0,
        shared_arc: Arc::clone(shared),
        sender,
    };
    // Each root opened on a thread of its own, the writer answering
    // meanwhile: none waits for another, a slow one for itself alone.
    for (i, path) in config.roots.iter().enumerate() {
        w.open_later(i, path.clone(), since, ex.clone());
    }
    w.forgotten_removed();
    w.volumes_followed();
    w.apply_scopes();
    turns(&mut w, commands);
    w.flush_all();
    for keeper in w.keepers.iter_mut().flatten() {
        if keeper.save().is_err() {
            log("a save at exit failed: the log keeps what changed");
        }
    }
}

/// The folders registered, as the system resolves them; one not there yet
/// is left out until it is registered again.
fn resolved(registrations: &[Registration]) -> Vec<PathBuf> {
    // In the order they were registered, the first of each kept: the
    // share of watches fills in that order.
    let mut seen = std::collections::HashSet::new();
    registrations
        .iter()
        .filter_map(|r| skwird_crawl::canonical(r.path.to_path()).ok())
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Open root `i`, at `path`: its index read back, or crawled; published
/// once ready. Says whether it was built.
#[allow(clippy::too_many_arguments)] // A root's own: one call, of the writer's state.
fn open_one(
    config: &Config,
    shared: &Shared,
    i: usize,
    path: &Path,
    dir: &Path,
    ex: &Exclusions,
    opts: &Options,
    cancel: &Cancel,
) -> (Option<Keeper>, bool) {
    let start = Instant::now();
    // The system service builds an index on NTFS from its volume's file
    // table (Windows' alone).
    #[cfg(windows)]
    let table = |root: &Root, opts: &Options, cancel: &Cancel, limits| {
        from_table(i, root, opts, cancel, limits)
    };
    #[cfg(windows)]
    let builder: Option<skwird_index::keeper::Builder> = config
        .system
        .then_some(&table as skwird_index::keeper::Builder);
    #[cfg(not(windows))]
    let builder = {
        let _ = config;
        None
    };
    let opened = Root::open(path, ex.clone()).and_then(|root| {
        Keeper::open_by(dir, root, opts.clone(), Limits::default(), cancel, builder)
    });
    match opened {
        Ok((keeper, how)) => {
            let sizes = keeper.index().sizes();
            log(&format!(
                "root {i} {} in {:.2?}: {} entries in {} folders",
                match &how {
                    Opened::Loaded { replayed, .. } =>
                        format!("loaded, {replayed} records replayed"),
                    Opened::Built { because: None } => "built".to_owned(),
                    Opened::Built { because: Some(why) } => format!("built again ({why})"),
                },
                start.elapsed(),
                sizes.entries,
                sizes.folders
            ));
            let built = match how {
                Opened::Loaded { reconciled, .. } => {
                    log_reconciled(i, reconciled);
                    false
                }
                Opened::Built { .. } => true,
            };
            shared.publish(i, Published::Ready(Arc::new(keeper.snapshot())));
            (Some(keeper), built)
        }
        Err(e) => {
            log(&format!("root {i} cannot be opened: {e}"));
            shared.publish(i, Published::Failed(e.to_string()));
            (None, false)
        }
    }
}

/// Root `i`'s index built from its volume's file table, read whole from
/// the disk in one pass, where a walk opens every folder (the user's
/// choice of 2026-11-23, `docs/PLAN.md`, phase 4): on NTFS, for the
/// system service, which may read a volume. Elsewhere, or where the table
/// cannot be read, the folders are walked.
#[cfg(windows)]
fn from_table(
    i: usize,
    root: &Root,
    opts: &Options,
    cancel: &Cancel,
    limits: Limits,
) -> Result<skwird_index::build::Built, skwird_crawl::Error> {
    use skwird_index::build::{crawl_and_build, crawl_and_build_by};
    if root.fs_name() != "NTFS" {
        return crawl_and_build(root, opts, cancel, limits);
    }
    let start = Instant::now();
    let table = match skwird_crawl::journal::Volume::table_of(root.path()) {
        Ok(table) => table,
        Err(e) => {
            log(&format!(
                "root {i}: its volume's file table cannot be read, so its folders are walked: {e}"
            ));
            return crawl_and_build(root, opts, cancel, limits);
        }
    };
    log(&format!(
        "root {i}: its volume's file table read in {:.2?}: {} records",
        start.elapsed(),
        table.len()
    ));
    // A record it could not read is in no folder it lists, and the journal
    // will not say it again (rule 1).
    if !table.unread.is_empty() {
        log(&format!(
            "root {i}: {} records of its volume's file table cannot be read, so its folders are walked",
            table.unread.len()
        ));
        return crawl_and_build(root, opts, cancel, limits);
    }
    let reader = skwird_crawl::mft::TableReader { table: &table };
    crawl_and_build_by(root, opts, cancel, limits, &|root, rel, id, opts| {
        reader.read(root, rel, id, opts)
    })
}

/// What the writer holds.
struct Writing<'a> {
    config: &'a Config,
    shared: &'a Shared,
    cancel: Cancel,
    /// How every root is read: a root mounted since is read so too.
    opts: Options,
    keepers: Vec<Option<Keeper>>,
    /// Folders clients keep out of the index.
    registrations: Vec<Registration>,
    /// Folders clients keep live.
    kept_live: Vec<Registration>,
    watcher: Option<Arc<Watcher>>,
    /// The system service's group (fanotify), and the mount table its
    /// handles are opened through, read again when it changes.
    group: Option<Arc<Group>>,
    table: Table,
    /// Each root's filesystem marked, by its device: none if it is not,
    /// and the root is checked, never live.
    marked: Vec<Option<(u32, u32)>>,
    /// Each folder heard found in an index, kept while finding it again
    /// would give the same ([`Keeper::holds`]): a busy folder is not found
    /// again for each event (DESIGN 8). At most [`FOUND_KEPT`].
    found: HashMap<fanotify::Folder, Found>,
    /// Each folder heard found in no index, in a root being opened, or
    /// gone ([`Unfound`]): kept while that holds, so that a busy folder kept
    /// out (the engine's own, the Nix store), or one heard while its root is
    /// crawled, is not found again for each batch of its events. Let go of
    /// whenever a root comes or goes, and the registrations or the mounts
    /// change. At most [`FOUND_KEPT`].
    unfound: HashMap<fanotify::Folder, Unfound>,
    /// What was heard and not applied yet, and what is due.
    gather: Gather,
    /// The folder cookies are made in, once watched.
    cookies: Option<PathBuf>,
    next_cookie: u64,
    /// Syncs waiting for their cookie.
    waiting: Vec<Waiting>,
    /// The removable volumes clients keep, those mounted, those released
    /// (`crate::volumes`): none kept at the system service.
    volumes: crate::volumes::State,
    /// The kept volumes that are roots, by identity.
    volume_roots: HashMap<String, VolumeRoot>,
    /// Syncs whose cookie was heard, to be served once what was heard is
    /// found ([`Writing::serve_heard`]): a client's ask, not charged.
    served: Vec<Waiting>,
    /// The processor time spent serving clients inside a turn that was not
    /// theirs (a sync served when its cookie is heard): left out of what
    /// the turn is charged (`turns`).
    for_clients: std::time::Duration,
    /// What was done for what was heard, since the last save.
    done: Done,
    /// Commands taken while gathering syncs, to answer next.
    pending: Vec<Command>,
    /// The roots being opened, each on a thread of its own
    /// ([`Writing::open_later`]): the writer answers meanwhile, and a slow
    /// root (a spinning disk) delays itself alone.
    opening: HashMap<usize, Opening>,
    /// What was heard that no root open could take, while roots are
    /// opening: handled again as each is. At most [`HELD_MOST`].
    held: Vec<HeldEvent>,
    /// Syncs of roots being opened, answered once each is.
    parked: Vec<Command>,
    /// Asked to stop while roots were opening: stopping once they are, so
    /// that a first build is kept.
    stopping: bool,
    /// How often clients' registrations changed: a root opened meanwhile
    /// is held to them again.
    registered: u64,
    /// How many openings and checks of roots on threads of their own were
    /// begun: each's [`Opening::generation`].
    openings: u64,
    /// For the threads that open roots: the shared state, and where they
    /// send what they opened.
    shared_arc: Arc<Shared>,
    sender: Sender<Command>,
    /// The roots' histories: the system service's journals on Windows, a
    /// user's engine's on macOS (`FSEvents`).
    #[cfg(any(windows, target_os = "macos"))]
    journals: Journals,
}

/// What the writer keeps of its roots' histories (DESIGN 2): Windows'
/// system service's journals, a user's engine's events on macOS. Where
/// each was read up to, to keep with its index once every change before
/// it is applied, and which roots prove nothing yet. A position kept is
/// never past a change not applied.
#[cfg(any(windows, target_os = "macos"))]
#[derive(Default)]
struct Journals {
    /// Each root's last position heard, not kept yet.
    pending: HashMap<usize, Kept>,
    /// The roots whose positions prove nothing until they are built or
    /// checked: one that could not resume, or whose changes were dropped
    /// (an overflow, a watch ended, a change not applied).
    unproven: std::collections::HashSet<usize>,
    /// The roots resumed from a position kept, live once their replay
    /// ends, from when they were followed.
    replaying: HashMap<usize, Time>,
    /// Each followed root's volume, held to be told when its removal is
    /// asked (`crate::service::hold_volume`).
    #[cfg(windows)]
    held: HashMap<usize, crate::service::Held>,
    /// The registrations of the kept volumes whose eject was asked, by
    /// identity, kept while their release lasts (a user's engine): one
    /// refused is told there.
    #[cfg(windows)]
    ejecting: HashMap<String, skwird_crawl::RemovalHeard>,
}

/// A position in a root's history: its journal's on Windows, its
/// volume's events' on macOS.
#[cfg(windows)]
type Kept = skwird_crawl::journal::Position;
#[cfg(target_os = "macos")]
type Kept = skwird_crawl::Position;

/// What a root's history is called in the log.
#[cfg(windows)]
const HISTORY: &str = "journal";
#[cfg(target_os = "macos")]
const HISTORY: &str = "history";

/// A position as the index keeps it.
#[cfg(windows)]
fn stored(at: Kept) -> skwird_index::store::Position {
    skwird_index::store::Position {
        history: at.journal,
        next: at.next,
    }
}

#[cfg(target_os = "macos")]
fn stored(at: Kept) -> skwird_index::store::Position {
    skwird_index::store::Position {
        history: at.database,
        // Past what the index keeps, a position ahead of every event: the
        // next start does not resume from it.
        next: i64::try_from(at.id).unwrap_or(i64::MAX),
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl Journals {
    /// The journals of `roots` roots, those `resuming` resumed from their
    /// positions kept at `since`: live once their replays end. One that is
    /// not proves nothing it hears until it is built or checked.
    fn started(roots: usize, resuming: &[usize], since: Time) -> Journals {
        let mut journals = Journals::default();
        for i in 0..roots {
            if resuming.contains(&i) {
                log(&format!(
                    "root {i}: resumed from its {HISTORY}'s position kept"
                ));
                journals.replaying.insert(i, since);
            } else {
                journals.unproven.insert(i);
            }
        }
        journals
    }

    /// Root `i`'s changes were dropped, or not applied: no position it
    /// hears is kept until it is checked, and its replay, cut short, ends
    /// with the check.
    fn doubt(&mut self, i: usize) {
        self.unproven.insert(i);
        self.pending.remove(&i);
        self.replaying.remove(&i);
    }
}

/// A root being opened: since when its filesystem was marked, and whether
/// what was held for it overflowed, so that it is checked before live.
struct Opening {
    since: Time,
    overflowed: bool,
    /// The writer's count of registrations changed when it began.
    registered: u64,
    /// Which opening of the writer's it is: a root let go of and found
    /// again is the same root, and the late `Opened` of what was let go of
    /// is not this one's.
    generation: u64,
    /// A kept volume's check, not its opening ([`Writing::check_later`]),
    /// begun at `since`.
    check: Option<Check>,
}

/// A kept volume's check on a thread of its own: since when its tree was
/// watched again first, if it was, and the syncs it serves.
struct Check {
    rewatched: Option<Time>,
    replies: Vec<Sender<()>>,
}

/// An event held while roots are opening, with its source's generation.
enum HeldEvent {
    Marks(u64, fanotify::Heard),
    Watched(u64, Heard),
}

/// The most events held while roots are opening: past them, every root
/// being opened is checked once it is, before it is live.
const HELD_MOST: usize = 65_536;

impl Writing<'_> {
    /// Answer one command: false to stop.
    #[allow(clippy::too_many_lines)] // One arm a command: the dispatcher of them all.
    fn answer(&mut self, commands: &Receiver<Command>, command: Command) -> bool {
        match command {
            Command::Stop => return false,
            Command::Opened {
                root,
                generation,
                keeper,
                built,
                current,
            } => self.opened(root, generation, keeper, built, current),
            // A root being opened is synced once it is.
            Command::Sync { root, rel, reply } if self.opening.contains_key(&root) => {
                self.parked.push(Command::Sync { root, rel, reply });
            }
            Command::Sync { root, rel, reply } => {
                // Every sync asked of this root meanwhile is served alike;
                // others wait their turn.
                let mut replies = vec![reply];
                let mut live = self.is_live(root, &rel);
                while let Ok(next) = commands.try_recv() {
                    match next {
                        Command::Sync {
                            root: r,
                            rel,
                            reply,
                        } if r == root => {
                            live &= self.is_live(r, &rel);
                            replies.push(reply);
                        }
                        other => self.pending.insert(0, other),
                    }
                }
                // A line added to a user's file holds from the next sync,
                // served by the barrier or by the check (DESIGN 5). A line
                // taken out, by the check of each root it gives folders back
                // to, as a registration taken back is: no barrier proves
                // what no one read, and what is heard there is not taken.
                let (readmitting, _) = self.hold_to_exclusions();
                live &= !readmitting.contains(&root);
                for i in readmitting.into_iter().filter(|&i| i != root) {
                    self.check_root(i);
                }
                // Live, a barrier serves: a cookie heard in its turn says
                // every change before it was heard (DESIGN 2). Otherwise,
                // the check.
                if live && let Some(cookie) = self.make_cookie(root) {
                    self.shared.readers.wake();
                    self.waiting.push(Waiting {
                        cookie,
                        root,
                        replies,
                        until: Instant::now() + BARRIER,
                    });
                } else {
                    self.check_now(root, replies);
                }
            }
            Command::Changes {
                root,
                rel,
                since,
                reply,
            } => {
                // Since nothing: from now, no event, and the token to go on
                // from, which a client takes before it walks.
                let answer = match (self.keepers[root].as_ref(), Token::parse(&since)) {
                    (Some(keeper), _) if since.is_empty() => keeper
                        .token()
                        .map(|t| (Vec::new(), t, keeper.complete(&rel, t)))
                        .ok_or(TooOld),
                    (Some(keeper), Some(token)) => keeper
                        .changes(&rel, token)
                        .map(|(events, now)| (events, now, keeper.complete(&rel, token))),
                    _ => Err(TooOld),
                };
                let _ = reply.send(answer);
            }
            Command::Exclude {
                client,
                uid,
                add,
                remove,
                reply,
            } => {
                self.register(&client, uid, &add, &remove);
                let _ = reply.send(theirs(&self.registrations, &client, uid));
            }
            Command::Volumes {
                client,
                keep,
                forget,
                release,
                release_at,
                reply,
            } => {
                let answered = self.volumes_asked(&client, &keep, &forget, &release, &release_at);
                let _ = reply.send(answered);
            }
            Command::Watch {
                client,
                add,
                remove,
                reply,
            } => {
                if changed(&mut self.kept_live, &client, None, &add, &remove) {
                    save_registered(&self.config.dirs, WATCHED, &self.kept_live);
                    self.registered += 1;
                    self.unfound.clear();
                    self.apply_scopes();
                }
                let _ = reply.send(self.watching(&client));
            }
            // What finding what was heard takes is charged to the account
            // (`live::DUTY`), as applying it is.
            Command::Heard { generation, heard } => {
                let start = thread_time();
                self.heard(generation, heard);
                self.noticing(start);
                self.serve_heard();
            }
            Command::Marks { generation, heard } => {
                let start = thread_time();
                self.noticed(generation, heard);
                self.noticing(start);
                self.serve_heard();
            }
            Command::Mounts => self.mounts_changed(),
            #[cfg(windows)]
            Command::LetGo { root, reply } => self.let_go_of_volume(root, &reply),
            #[cfg(windows)]
            Command::EjectRefused { id } => self.eject_refused(&id),
        }
        true
    }

    /// What the roots' watches heard (Windows, phase 4, step 5): each
    /// entry's folder found in its root's index by its path, and gathered;
    /// an overflow lowering its root's coverage at once, the check due once
    /// the queue is quiet; a watch that ended, its root no longer heard.
    #[cfg(windows)]
    fn heard(&mut self, generation: u64, heard: Vec<Heard>) {
        let now = Instant::now();
        self.done.events += heard.len() as u64;
        let mut lowered = BTreeSet::new();
        for h in heard {
            match h {
                Heard::Entry { scope, .. }
                | Heard::Position { scope, .. }
                | Heard::Replayed { scope }
                    if self.opening.contains_key(&scope) =>
                {
                    self.hold(HeldEvent::Watched(generation, h));
                }
                Heard::Entry { scope, rel } => {
                    if let Some((folder, name)) = self.entry_heard(scope, &rel) {
                        match name {
                            Some(name) => self.gather.note_entry(scope, folder, name, now),
                            None => self.gather.note(scope, folder, now),
                        }
                    }
                }
                Heard::Overflow { scope } => {
                    self.done.overflows += 1;
                    self.journals.doubt(scope);
                    self.opening_overflowed();
                    if let Some(keeper) = self.keepers.get_mut(scope).and_then(Option::as_mut) {
                        keeper.overflowed();
                        lowered.insert(scope);
                    }
                }
                Heard::Lost { scope } => {
                    log(&format!(
                        "root {scope}: its watch ended, so it is checked, never live"
                    ));
                    self.journals.doubt(scope);
                    if let Some(m) = self.marked.get_mut(scope) {
                        *m = None;
                    }
                    if let Some(keeper) = self.keepers.get_mut(scope).and_then(Option::as_mut) {
                        keeper.unheard();
                        lowered.insert(scope);
                    }
                }
                Heard::Cookie(n) => {
                    // Only the current watcher's cookie proves its queue read.
                    if self
                        .watcher
                        .as_ref()
                        .is_some_and(|w| w.generation() == generation)
                    {
                        self.cookie_heard(n);
                    }
                }
                // Kept once every change heard before it is applied, if the
                // root proves what it hears.
                Heard::Position { scope, at } => {
                    if !self.journals.unproven.contains(&scope) {
                        self.journals.pending.insert(scope, at);
                    }
                }
                Heard::Replayed { scope } => self.replayed(scope),
            }
        }
        if !lowered.is_empty() {
            log("events dropped: not live until checked");
            self.gather.overflowed(now);
            for i in lowered {
                self.publish(i);
            }
        }
    }

    /// Where an entry heard at `rel` below root `scope` is to be read
    /// again: its folder, and its name to stat; or, its folder not in the
    /// index yet (made since, with what is in it), the nearest folder above
    /// it the index holds, read whole, its new folders crawled. None where
    /// that folder is not covered (the engine's own, an exclusion, a
    /// mount): nothing heard there is read.
    #[cfg(windows)]
    fn entry_heard(&self, scope: usize, rel: &Path) -> Option<(FolderId, Option<Box<[u8]>>)> {
        let index = self.keepers.get(scope)?.as_ref()?.index();
        let covered = |f: FolderId| {
            !matches!(
                index.state_of(f),
                Some(skwird_index::block::State::NotCovered(_))
            )
        };
        let parent = rel.parent().unwrap_or(Path::new(""));
        if let Some(folder) = index.folder_at(parent) {
            let mut name = Vec::new();
            skwird_crawl::wtf8::push(Path::new(rel.file_name()?), &mut name);
            return covered(folder).then(|| (folder, Some(name.into_boxed_slice())));
        }
        let mut above = parent.to_path_buf();
        let folder = loop {
            if !above.pop() {
                break index.root()?;
            }
            if let Some(f) = index.folder_at(&above) {
                break f;
            }
        };
        covered(folder).then_some((folder, None))
    }

    /// What the roots' stream heard (macOS, phase 5, step 3): each folder
    /// named found in its root's index by its path, and gathered; an
    /// overflow lowering its root's coverage at once, the check due once
    /// the queue is quiet; a root no longer heard, not live.
    #[cfg(target_os = "macos")]
    fn heard(&mut self, generation: u64, heard: Vec<Heard>) {
        let now = Instant::now();
        self.done.events += heard.len() as u64;
        let mut lowered = BTreeSet::new();
        for h in heard {
            match h {
                Heard::Folder { scope, .. }
                | Heard::Position { scope, .. }
                | Heard::Replayed { scope }
                    if self.opening.contains_key(&scope) =>
                {
                    self.hold(HeldEvent::Watched(generation, h));
                }
                Heard::Folder { scope, rel } => {
                    if let Some(folder) = self.folder_heard(scope, &rel) {
                        self.gather.note(scope, folder, now);
                    }
                }
                Heard::Overflow { scope } => {
                    self.done.overflows += 1;
                    self.journals.doubt(scope);
                    self.opening_overflowed();
                    if let Some(keeper) = self.keepers.get_mut(scope).and_then(Option::as_mut) {
                        keeper.overflowed();
                        lowered.insert(scope);
                    }
                }
                Heard::Lost { scope } => {
                    log(&format!(
                        "root {scope}: its watch ended, so it is checked, never live"
                    ));
                    self.journals.doubt(scope);
                    if let Some(m) = self.marked.get_mut(scope) {
                        *m = None;
                    }
                    if let Some(keeper) = self.keepers.get_mut(scope).and_then(Option::as_mut) {
                        keeper.unheard();
                        lowered.insert(scope);
                    }
                }
                Heard::Cookie(n) => {
                    // Only the current watcher's cookie proves its queue read.
                    if self
                        .watcher
                        .as_ref()
                        .is_some_and(|w| w.generation() == generation)
                    {
                        self.cookie_heard(n);
                    }
                }
                // Kept once every change heard before it is applied, if the
                // root proves what it hears. Not while it replays: the
                // roots' stream is ahead of its replay, and a position kept
                // past history not replayed would lose it at the next
                // start.
                Heard::Position { scope, at } => {
                    if !self.journals.unproven.contains(&scope)
                        && !self.journals.replaying.contains_key(&scope)
                    {
                        self.journals.pending.insert(scope, at);
                    }
                }
                Heard::Replayed { scope } => self.replayed(scope),
            }
        }
        if !lowered.is_empty() {
            log("events dropped: not live until checked");
            self.gather.overflowed(now);
            for i in lowered {
                self.publish(i);
            }
        }
    }

    /// The folder heard at `rel` below root `scope`, to be read again: as
    /// the index holds it, or, not in it yet (made since), the nearest
    /// folder above it the index holds, read whole, its new folders
    /// crawled. None where that folder is not covered (the engine's own, an
    /// exclusion, a mount): nothing heard there is read.
    #[cfg(target_os = "macos")]
    fn folder_heard(&self, scope: usize, rel: &Path) -> Option<FolderId> {
        let index = self.keepers.get(scope)?.as_ref()?.index();
        let mut at = rel.to_path_buf();
        let folder = loop {
            if let Some(f) = index.folder_at(&at) {
                break f;
            }
            if !at.pop() {
                break index.root()?;
            }
        };
        let covered = !matches!(
            index.state_of(folder),
            Some(skwird_index::block::State::NotCovered(_))
        );
        covered.then_some(folder)
    }

    /// What the watcher heard: each folder named gathered, an overflow or
    /// a watched subtree's own folder lost lowering coverage at once, the
    /// check due once the queue is quiet.
    #[cfg(target_os = "linux")]
    fn heard(&mut self, generation: u64, heard: Vec<Heard>) {
        let now = Instant::now();
        self.done.events += heard.len() as u64;
        // An older watcher's changes are applied all the same (a read is
        // never wrong), but its cookies prove nothing (DESIGN 2).
        let mut lowered = BTreeSet::new();
        for h in heard {
            // A watch no root open knows of, while roots are opening, may
            // be one of theirs: held until they are.
            let watch = match &h {
                Heard::Changed(wd) | Heard::Entry(wd, _) | Heard::Moved(wd) | Heard::Gone(wd) => {
                    Some(*wd)
                }
                _ => None,
            };
            if let Some(wd) = watch
                && !self.opening.is_empty()
                && !self
                    .keepers
                    .iter()
                    .flatten()
                    .any(|k| k.watched(wd).is_some())
            {
                self.hold(HeldEvent::Watched(generation, h));
                continue;
            }
            match h {
                Heard::Changed(wd) => {
                    for (i, keeper) in self.keepers.iter().enumerate() {
                        if let Some(folder) = keeper.as_ref().and_then(|k| k.watched(wd)) {
                            self.gather.note(i, folder, now);
                        }
                    }
                }
                Heard::Entry(wd, name) => {
                    for (i, keeper) in self.keepers.iter().enumerate() {
                        if let Some(folder) = keeper.as_ref().and_then(|k| k.watched(wd)) {
                            self.gather.note_entry(i, folder, name.clone(), now);
                        }
                    }
                }
                Heard::Moved(wd) | Heard::Gone(wd) => {
                    for (i, keeper) in self.keepers.iter_mut().enumerate() {
                        let Some(keeper) = keeper else { continue };
                        if let Some(folder) = keeper.watched(wd)
                            && keeper.lost(folder)
                        {
                            lowered.insert(i);
                        }
                        if matches!(h, Heard::Gone(_)) {
                            keeper.forget(wd);
                        }
                    }
                }
                Heard::Cookie(n) => {
                    // Only the current watcher's cookie proves its queue read.
                    if self
                        .watcher
                        .as_ref()
                        .is_some_and(|w| w.generation() == generation)
                    {
                        self.cookie_heard(n);
                    }
                }
                Heard::Overflow => {
                    self.done.overflows += 1;
                    self.opening_overflowed();
                    for (i, keeper) in self.keepers.iter_mut().enumerate() {
                        if let Some(keeper) = keeper {
                            keeper.overflowed();
                            lowered.insert(i);
                        }
                    }
                }
            }
        }
        if !lowered.is_empty() {
            log("events dropped, or a watched folder lost: not live until checked");
            self.gather.overflowed(now);
            for i in lowered {
                self.publish(i);
            }
        }
    }

    /// What finding what was heard took, since `start`: counted, and
    /// charged to the account.
    fn noticing(&mut self, start: std::time::Duration) {
        let took = thread_time().saturating_sub(start);
        self.done.noticing += took;
        self.gather.spent(took, Instant::now());
    }

    /// What is due now: gatherings done applied, the check after an
    /// overflow, a save.
    fn on_time(&mut self) {
        let now = Instant::now();
        if self.volumes.released.values().any(|until| *until <= now) {
            self.volumes_followed();
        }
        // A cookie not heard in time: the barrier failed, the check serves.
        let (late, waiting): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| now >= w.until);
        self.waiting = waiting;
        for w in late {
            // A client's ask too, served by the check: not charged.
            let start = thread_time();
            self.forget_cookie(w.cookie);
            self.check_now(w.root, w.replies);
            let took = thread_time().saturating_sub(start);
            self.done.syncing += took;
            self.for_clients += took;
        }
        // A batch at a time, each charged before the next is taken. Timed by
        // this thread's own processor clock, so that applying what is heard
        // keeps to its share of a core (`live::DUTY`): the waits of a busy
        // disk are not the engine's processor time.
        while let Some((root, folders)) = self.gather.due(Instant::now()) {
            let start = thread_time();
            let read = self.apply_heard(root, &folders);
            let took = thread_time().saturating_sub(start);
            self.done.applying += took;
            self.done.reads_took(&read, took);
            self.gather.applied(root, &read, took, Instant::now());
        }
        #[cfg(any(windows, target_os = "macos"))]
        self.positions_applied();
        if self.gather.recheck_due(now) {
            let start = thread_time();
            for i in 0..self.keepers.len() {
                self.check_root(i);
            }
            // Charged, not held: it is what makes the roots live again.
            self.gather
                .spent(thread_time().saturating_sub(start), Instant::now());
        }
        if self.gather.save_due(now) {
            self.done.say();
            let start = thread_time();
            for keeper in self.keepers.iter_mut().flatten() {
                if keeper.save().is_err() {
                    log("a save failed: the index is kept in memory, the log on disk");
                }
            }
            let saved = thread_time();
            crate::memory::give_back();
            let end = thread_time();
            self.done.saving += saved.saturating_sub(start);
            self.done.giving_back += end.saturating_sub(saved);
            self.gather.saved(end.saturating_sub(start), Instant::now());
        }
    }

    /// Every gathering applied at once, as the engine stops, and the
    /// positions heard before it kept, to be saved.
    fn flush_all(&mut self) {
        for root in 0..self.keepers.len() {
            let heard = self.gather.take(root);
            self.apply_heard(root, &heard);
        }
        #[cfg(any(windows, target_os = "macos"))]
        self.positions_applied();
    }

    /// The folders of `root` heard, read again; gives those read, each with
    /// the entries its listing held.
    fn apply_heard(&mut self, root: usize, folders: &[HeardFolder]) -> Vec<(FolderId, usize)> {
        if folders.is_empty() {
            return Vec::new();
        }
        let Some(keeper) = self.keepers[root].as_mut() else {
            return Vec::new();
        };
        let read = match keeper.heard_named(folders, &self.cancel) {
            Ok(done) => {
                self.done.read_again(root, &done);
                if !done.records.is_empty() {
                    self.gather.changed(Instant::now());
                }
                done.folders
            }
            Err(e) => {
                log(&format!(
                    "root {root}: what was heard cannot be applied: {e}"
                ));
                // No position past it is kept until the root is checked.
                #[cfg(any(windows, target_os = "macos"))]
                {
                    self.journals.unproven.insert(root);
                    self.journals.pending.remove(&root);
                }
                Vec::new()
            }
        };
        self.publish(root);
        read
    }

    fn publish(&self, root: usize) {
        if let Some(keeper) = &self.keepers[root] {
            self.shared
                .publish(root, Published::Ready(Arc::new(keeper.snapshot())));
        }
    }

    /// The folders clients keep live, watched: the watcher told, each root
    /// given those below it, each new one installed.
    fn apply_scopes(&mut self) {
        // A root heard whole: no folder is kept live alone.
        if HEARD_WHOLE {
            return;
        }
        let Some(watcher) = &self.watcher else {
            return;
        };
        let scopes = resolved(&self.kept_live);
        watcher.set_scopes(scopes.clone());
        for root in 0..self.keepers.len() {
            let Some(keeper) = self.keepers[root].as_mut() else {
                continue;
            };
            let base = keeper.root().path().to_path_buf();
            let rels: Vec<PathBuf> = scopes
                .iter()
                .filter_map(|s| s.strip_prefix(&base).ok().map(Path::to_path_buf))
                .collect();
            let start = Instant::now();
            match keeper.set_scopes(&rels, &self.cancel) {
                Ok(done) if done.checked > 0 => log(&format!(
                    "root {root}: {} folders watched in {:.2?}, {} read again, {} crawled",
                    watcher.watching(),
                    start.elapsed(),
                    done.reread,
                    done.crawled
                )),
                Ok(_) => {}
                Err(e) => log(&format!("root {root}: its folders cannot be watched: {e}")),
            }
            for live in keeper.live() {
                if let Some(short) = live.short {
                    log(&format!(
                        "root {root}: a folder kept live is checked only: {short:?}"
                    ));
                }
            }
            self.publish(root);
        }
    }

    /// What `client` keeps live, in the order it kept it, each folder live
    /// or why not, and the share of watches every client's folders fill.
    fn watching(&self, client: &str) -> skwird_protocol::Watching {
        use skwird_protocol::{Keeping, Kept, Share, Watching};
        // Every client's folders, as resolved, each with the path it was
        // first kept by: what may hold one of this client's.
        let mut seen = std::collections::HashSet::new();
        let all: Vec<(PathBuf, WirePath)> = self
            .kept_live
            .iter()
            .filter_map(|r| {
                Some((
                    skwird_crawl::canonical(r.path.to_path()).ok()?,
                    r.path.clone(),
                ))
            })
            .filter(|(p, _)| seen.insert(p.clone()))
            .collect();
        let folders = self
            .kept_live
            .iter()
            .filter(|r| r.client == client && r.uid.is_none())
            .map(|r| {
                let resolved = skwird_crawl::canonical(r.path.to_path()).ok();
                let (state, folders) = match &resolved {
                    Some(p) => self.keeping(p),
                    None => (Keeping::NotLive(skwird_protocol::NotLive::Missing), None),
                };
                // Held by another folder kept live: it takes no watch of
                // its own.
                let within = resolved.as_ref().and_then(|p| {
                    all.iter()
                        .filter(|(o, _)| o != p && p.starts_with(o))
                        .filter(|(o, _)| matches!(self.keeping(o).0, Keeping::Live(_)))
                        .max_by_key(|(o, _)| o.components().count())
                        .map(|(_, path)| path.clone())
                });
                Kept {
                    path: r.path.clone(),
                    state,
                    folders,
                    within,
                }
            })
            .collect();
        let (used, size) = self
            .watcher
            .as_ref()
            .map_or((0, 0), |w| (w.watching(), w.budget()));
        Watching {
            folders,
            // Each root heard whole, with no share of watches.
            share: (!HEARD_WHOLE).then_some(Share {
                used: used as u64,
                size: size as u64,
            }),
        }
    }

    /// Whether the folder at `path`, as resolved, kept live, is, or why
    /// not, and the folders its watches take.
    fn keeping(&self, path: &Path) -> (skwird_protocol::Keeping, Option<u64>) {
        use skwird_crawl::Watch;
        use skwird_protocol::{Keeping, NotLive, Proof, Why};
        let Some((i, rel)) = self.shared.locate(path) else {
            return (Keeping::NotLive(NotLive::NotCovered(Why::Outside)), None);
        };
        let Some(keeper) = self.keepers.get(i).and_then(Option::as_ref) else {
            return (Keeping::NotLive(NotLive::Pending), None);
        };
        let index = keeper.index();
        let folders = index.folder_at(&rel).map(|f| index.folders_below(f));
        if self.watcher.is_none() {
            // No watcher to be had: the system's limit of instances.
            return (Keeping::NotLive(NotLive::WatchLimit), folders);
        }
        let live = keeper.live().iter().find(|l| l.scope == rel);
        let not_live = match live.map(|l| (l.since, l.short)) {
            Some((Some(since), None)) => {
                return (Keeping::Live(crate::convert::time(since)), folders);
            }
            Some((_, Some(Watch::Budget))) => NotLive::Share,
            Some((_, Some(Watch::Limit))) => NotLive::WatchLimit,
            Some((_, Some(Watch::Failed(errno)))) => NotLive::Failed(errno),
            _ => match crate::coverage::coverage_at(index, keeper.root().path(), &rel) {
                None => NotLive::Missing,
                Some(c) => match c.names {
                    // In a root heard whole (Windows), live as it is.
                    Proof::Live(since) => return (Keeping::Live(since), folders),
                    Proof::NotCovered(why) => NotLive::NotCovered(why),
                    _ => NotLive::Pending,
                },
            },
        };
        (Keeping::NotLive(not_live), folders)
    }

    /// Every root made to hold to the exclusions as they are now: the
    /// defaults, the user's file read again, what clients registered. A
    /// folder they name is purged at once. Gives back the roots whose
    /// folders are to be read again, and how many exclusions were refused.
    fn hold_to_exclusions(&mut self) -> (Vec<usize>, usize) {
        let (ex, refused) = exclusions(self.config, &self.registrations);
        let mut readmitting = Vec::new();
        for (i, keeper) in self.keepers.iter_mut().enumerate() {
            let Some(keeper) = keeper else { continue };
            let r = keeper.set_exclusions(ex.clone());
            log_reconciled(i, r);
            if r.readmitted > 0 {
                readmitting.push(i);
            }
            if r.purged > 0 || r.readmitted > 0 {
                self.shared
                    .publish(i, Published::Ready(Arc::new(keeper.snapshot())));
            }
        }
        (readmitting, refused)
    }

    /// Take a client's registrations: what it adds is purged at once, what
    /// it takes back is read again. A client takes back only what it added.
    fn register(&mut self, client: &str, uid: Option<u32>, add: &[PathBuf], remove: &[PathBuf]) {
        // At the system service, a user registers only folders they own,
        // and at most so many: one refused is not kept, nor said kept.
        let add: Vec<PathBuf> = match uid {
            Some(uid) if self.config.system => {
                let users = crate::system::users();
                let home = users
                    .iter()
                    .find(|(u, _)| *u == uid)
                    .map_or_else(PathBuf::new, |(_, h)| h.clone());
                let held = self
                    .registrations
                    .iter()
                    .filter(|r| r.uid == Some(uid))
                    .count();
                add.iter()
                    .filter(|p| theirs_kept(&mut Exclusions::none(), uid, &home, p))
                    .take(MOST_REGISTERED.saturating_sub(held))
                    .cloned()
                    .collect()
            }
            _ => add.to_vec(),
        };
        if !changed(&mut self.registrations, client, uid, &add, remove) {
            return;
        }
        // Kept first: a kill before the purge's save leaves it to the next
        // open, which holds the index to what is kept.
        save_registered(&self.config.dirs, EXCLUDED, &self.registrations);
        self.registered += 1;
        self.unfound.clear();
        self.exclusions_held();
    }

    /// The index held to the exclusions derived again: what they add
    /// purged, the roots of what they take back read again.
    fn exclusions_held(&mut self) {
        let (readmitting, refused) = self.hold_to_exclusions();
        log_refused(refused);
        for i in readmitting {
            self.check_root(i);
        }
    }

    /// Root `i` checked ([`Writing::check_root_for`]).
    fn check_root(&mut self, i: usize) {
        self.check_root_for(i, Vec::new());
    }

    /// Root `i` checked, and the syncs `replies` answered once it is. On
    /// Windows and macOS, where its watch ended, its tree is watched again
    /// first ([`Writing::checked`]). A kept volume's is checked on a thread
    /// of its own ([`Writing::check_later`]), the syncs answered once it
    /// comes back: a stat of it can block on its disk, and a release cancels
    /// it.
    fn check_root_for(&mut self, i: usize, replies: Vec<Sender<()>>) {
        #[cfg(any(windows, target_os = "macos"))]
        let rewatched = self.rewatch(i);
        #[cfg(not(any(windows, target_os = "macos")))]
        let rewatched: Option<Time> = None;
        let from = Time::now();
        let volume = self
            .volume_roots
            .values()
            .find(|v| v.root == i)
            .map(|v| (v.cancel.clone(), v.reading.begin()));
        if let Some((cancel, read)) = volume {
            self.check_later(i, (cancel, read), rewatched, from, replies);
            return;
        }
        if let Some(keeper) = self.keepers[i].as_mut() {
            let current = refresh(keeper, i, self.config.system, self.shared, &self.cancel);
            self.checked(i, rewatched, from, current);
        }
        answered(replies);
    }

    /// What a check of root `i`, begun at `from`, proved: `current` or not.
    /// On Windows and macOS, its tree watched again from `rewatched`, where
    /// its watch had ended, is heard whole once the check proves it current:
    /// a root on a drive taken away and given back, or whose watch failed,
    /// is live again, not checked until the engine starts again; a watch
    /// whose check fails is let go of, to be tried again. A root heard whole
    /// and not live yet (its replay cut short, its check at opening failed)
    /// is live from `from`; and what its history says from then on proves
    /// again (DESIGN 2). On Linux, nothing more: a user's engine's roots are
    /// live by their scopes.
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        allow(clippy::unused_self) // Linux's has nothing more to do.
    )]
    fn checked(&mut self, i: usize, rewatched: Option<Time>, from: Time, current: bool) {
        #[cfg(any(windows, target_os = "macos"))]
        {
            let Some(keeper) = self.keepers[i].as_mut() else {
                return;
            };
            if let Some(since) = rewatched {
                if current {
                    keeper.heard_whole(since);
                    self.journals.unproven.remove(&i);
                    self.publish(i);
                    log(&format!("root {i}: watched again, live"));
                } else {
                    if let Some(w) = &self.watcher {
                        w.unwatch_root(i);
                    }
                    self.marked[i] = None;
                }
            } else if current {
                self.journals.unproven.remove(&i);
                let heard = self.marked.get(i).copied().flatten().is_some();
                if heard && keeper.live().is_empty() && !self.journals.replaying.contains_key(&i) {
                    keeper.heard_whole(from);
                    self.publish(i);
                    log(&format!("root {i}: checked, live"));
                }
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = (i, rewatched, from, current);
    }

    /// Root `scope`'s replay from its position kept is done (Windows, macOS):
    /// what changed while no engine ran has been heard, and it is live from
    /// when it was followed, as if no engine had stopped. Its check is not
    /// needed (DESIGN 2).
    #[cfg(any(windows, target_os = "macos"))]
    fn replayed(&mut self, scope: usize) {
        let Some(since) = self.journals.replaying.remove(&scope) else {
            return;
        };
        if let Some(keeper) = self.keepers.get_mut(scope).and_then(Option::as_mut) {
            keeper.heard_whole(since);
            self.publish(scope);
            log(&format!("root {scope}: its {HISTORY} replayed, live"));
        }
    }

    /// The positions heard of roots every change before which is applied
    /// (none of their folders heard waits), kept with their indexes, for
    /// the next start to resume from (Windows, macOS). Saved with the next save
    /// that something else makes due, or at a clean stop: never a save of
    /// its own, or the engine's own saves, heard in its volume's journal,
    /// would make one due after another.
    #[cfg(any(windows, target_os = "macos"))]
    fn positions_applied(&mut self) {
        let ready: Vec<usize> = self
            .journals
            .pending
            .keys()
            .copied()
            .filter(|&i| !self.gather.holds(i))
            .collect();
        for i in ready {
            let Some(keeper) = self.keepers.get_mut(i).and_then(Option::as_mut) else {
                continue;
            };
            let Some(at) = self.journals.pending.remove(&i) else {
                continue;
            };
            if keeper.position().is_none() {
                log(&format!("root {i}: its {HISTORY}'s position kept"));
            }
            keeper.set_position(Some(stored(at)));
        }
    }

    /// Root `i`'s volume held, to be told when its removal is asked
    /// (Windows' system service): where it is followed.
    #[cfg(windows)]
    fn hold_volume(&mut self, i: usize, path: &Path) {
        if !self.config.system || self.marked.get(i).copied().flatten().is_none() {
            return;
        }
        if let Some(held) = crate::service::hold_volume(i, path) {
            self.journals.held.insert(i, held);
        }
    }

    /// The removal of root `i`'s volume is asked (Windows): its watch and
    /// every handle of it let go of, so that the removal goes on, and
    /// `reply` answered. Not live
    /// from now; once its volume is back, it is checked and followed
    /// again ([`Writing::mounts_changed`]).
    #[cfg(windows)]
    fn let_go_of_volume(&mut self, i: usize, reply: &Sender<()>) {
        // A kept volume's eject asked (the taskbar's "Safely remove"): a
        // release, answered once its reads end, off the writer's turn.
        let kept = self
            .volume_roots
            .iter()
            .find(|(_, v)| v.root == i)
            .map(|(id, v)| (id.clone(), Arc::clone(&v.reading)));
        if let Some((id, reading)) = kept {
            self.volumes
                .released
                .insert(id.clone(), Instant::now() + crate::volumes::RELEASED_FOR);
            // Its registration kept while it is released: an eject refused
            // is told there, and takes it back at once.
            if let Some(held) = self.volume_roots.get_mut(&id).and_then(|v| v.held.take()) {
                self.journals.ejecting.insert(id.clone(), held);
            }
            self.let_go_of_kept(&id);
            let reply = reply.clone();
            std::thread::spawn(move || {
                reading.ended_within(crate::volumes::RELEASE_WAIT);
                let _ = reply.send(());
            });
            return;
        }
        self.let_go_of(i, "is being removed");
        let _ = reply.send(());
    }

    /// The eject of the kept volume `id` refused (Windows): its release
    /// ended, and it is followed again, a root at once.
    #[cfg(windows)]
    fn eject_refused(&mut self, id: &str) {
        if let Some(held) = self.journals.ejecting.remove(id) {
            std::thread::spawn(move || drop(held));
        }
        if self.volumes.released.remove(id).is_some() {
            log("a kept volume's eject was refused: it is followed again");
            self.volumes_followed();
        }
    }

    /// A kept volume's removal heard as it is asked (Windows): the writer
    /// asked to let go of root `i` (`Command::LetGo`), and waited for, 10 s
    /// at most, before the removal goes on; the kept volume `id` taken
    /// back if it is refused.
    #[cfg(windows)]
    fn removal_heard(&self, i: usize, id: &str, at: &Path) -> Option<skwird_crawl::RemovalHeard> {
        let sender = self.sender.clone();
        let refused = (self.sender.clone(), id.to_owned());
        let heard = skwird_crawl::RemovalHeard::new(
            at,
            Box::new(move || {
                let (reply, answer) = std::sync::mpsc::channel();
                if sender.send(Command::LetGo { root: i, reply }).is_ok() {
                    let _ = answer.recv_timeout(std::time::Duration::from_secs(10));
                }
            }),
            Box::new(move || {
                let (sender, id) = &refused;
                let _ = sender.send(Command::EjectRefused { id: id.clone() });
            }),
        );
        match heard {
            Ok(h) => Some(h),
            Err(e) => {
                log(&format!("root {i}: its removal is not heard: {e}"));
                None
            }
        }
    }

    /// Root `i`'s volume let go of, as it `goes` (Windows): its watch, its
    /// registration's handle, its folder and volume handles; not live.
    #[cfg(windows)]
    fn let_go_of(&mut self, i: usize, goes: &str) {
        if let Some(w) = &self.watcher {
            w.unwatch_root(i);
        }
        if let Some(m) = self.marked.get_mut(i) {
            *m = None;
        }
        // Its handle closed now; its registration ended once the
        // manager's call is over ([`Writing::mounts_changed`]).
        if let Some(held) = self.journals.held.get_mut(&i) {
            held.release();
        }
        self.journals.doubt(i);
        if let Some(keeper) = self.keepers.get_mut(i).and_then(Option::as_mut) {
            keeper.unheard();
            keeper.root().let_go();
        }
        self.publish(i);
        log(&format!("root {i}: its volume {goes}, let go of"));
    }

    /// The kept volumes mounted made roots, and those no longer mounted,
    /// kept, or let be (released) let go of (`crate::volumes`). A volume
    /// mounted where a root of the engine's own is, is that root's.
    fn volumes_followed(&mut self) {
        if self.config.system {
            return;
        }
        // One listing that fails says nothing of what is mounted: what was
        // seen last stands, or every kept volume would be let go of.
        match skwird_crawl::removable::mounted() {
            Ok(seen) => self.volumes.seen = seen,
            Err(e) => log(&format!(
                "the volumes mounted cannot be listed, so as last seen: {e}"
            )),
        }
        self.follow_seen();
    }

    /// [`Writing::volumes_followed`] by the volumes as last listed.
    fn follow_seen(&mut self) {
        let now = Instant::now();
        // A release ends as its volume is seen unmounted.
        let seen = &self.volumes.seen;
        self.volumes.released.retain(|id, until| {
            *until > now && seen.iter().any(|s| s.id.as_deref() == Some(id.as_str()))
        });
        // An eject's registration ends with its release, off the writer's
        // turn.
        #[cfg(windows)]
        {
            let released = &self.volumes.released;
            let ended: Vec<String> = self
                .journals
                .ejecting
                .keys()
                .filter(|id| !released.contains_key(*id))
                .cloned()
                .collect();
            for id in ended {
                if let Some(held) = self.journals.ejecting.remove(&id) {
                    std::thread::spawn(move || drop(held));
                }
            }
        }
        let rooted: HashMap<String, PathBuf> = self
            .volume_roots
            .iter()
            .map(|(id, v)| (id.clone(), v.at.clone()))
            .collect();
        let mut plan = crate::volumes::plan(
            &self.volumes.kept,
            &self.volumes.seen,
            &self.volumes.released,
            now,
            &rooted,
        );
        let own: Vec<PathBuf> = self.config.roots.clone();
        plan.roots.retain(|(_, at)| !own.contains(at));
        let gone: Vec<String> = self
            .volume_roots
            .iter()
            .filter(|(id, v)| !plan.roots.iter().any(|(i, at)| i == *id && *at == v.at))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.let_go_of_kept(&id);
        }
        let added: Vec<(String, PathBuf)> = plan
            .roots
            .iter()
            .filter(|(id, _)| !self.volume_roots.contains_key(id))
            .cloned()
            .collect();
        if !added.is_empty() {
            let (ex, refused) = exclusions(self.config, &self.registrations);
            log_refused(refused);
            for (id, at) in added {
                let i = self.shared.add_root(&at);
                while self.keepers.len() <= i {
                    self.keepers.push(None);
                    self.marked.push(None);
                }
                #[cfg(windows)]
                let held = self.removal_heard(i, &id, &at);
                self.volume_roots.insert(
                    id,
                    VolumeRoot {
                        root: i,
                        at: at.clone(),
                        cancel: Cancel::default(),
                        reading: Arc::default(),
                        #[cfg(windows)]
                        held,
                    },
                );
                log(&format!(
                    "root {i}: a kept volume mounted, a root from now on"
                ));
                // Heard whole from now on, as every root is on macOS: its
                // index, read back, is checked first. No history is resumed.
                #[cfg(target_os = "macos")]
                if let Some(w) = &self.watcher {
                    self.marked[i] = watch_whole(w, i, &at, None).0;
                }
                #[cfg(windows)]
                if let Some(w) = &self.watcher {
                    self.marked[i] = watch_whole(w, i, &at, false, None).0;
                }
                self.open_later(i, at, Time::now(), ex.clone());
            }
        }
        self.volumes.plan = plan;
    }

    /// The kept volume `id` a root no more: what reads it cancelled, its
    /// keeper let go of (its index kept), nothing held on it.
    fn let_go_of_kept(&mut self, id: &str) {
        let Some(v) = self.volume_roots.remove(id) else {
            return;
        };
        let i = v.root;
        v.cancel.cancel();
        // Those whose reads ended are waited for by none.
        self.volumes
            .let_go
            .retain(|(_, r)| !r.ended_within(std::time::Duration::ZERO));
        self.volumes
            .let_go
            .push((id.to_owned(), Arc::clone(&v.reading)));
        #[cfg(windows)]
        if let Some(held) = v.held {
            std::thread::spawn(move || drop(held));
        }
        // Heard whole where roots are; on Linux, the watches of the folders
        // clients keep live there go with the scopes, and a watch holds no
        // mount.
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(w) = &self.watcher {
            w.unwatch_root(i);
        }
        self.keepers[i] = None;
        self.marked[i] = None;
        self.shared.drop_root(i);
        // Being opened or checked, it is let go of as it comes back; its
        // syncs are answered now, as of a root that is none.
        self.opening_given_up(i);
        #[cfg(target_os = "linux")]
        self.apply_scopes();
        log(&format!("root {i}: a kept volume, a root no more"));
    }

    /// Root `i`'s opening or check given up, the root let go of: what its
    /// thread gives back is not taken ([`Writing::opened`]), and its syncs
    /// are answered now, as of a root that is none.
    fn opening_given_up(&mut self, i: usize) {
        if let Some(Opening {
            check: Some(check), ..
        }) = self.opening.remove(&i)
        {
            answered(check.replies);
        }
        self.unpark(i);
    }

    /// The indexes of volumes no client keeps removed, each once no read of
    /// it runs: a read of one forgotten may save it after it is forgotten.
    /// Derived from what clients keep, at each start too, so that one whose
    /// removal a stop or a kill cut short, or forgotten while no engine ran,
    /// is removed at the next (AGENTS.md, "What follows from a list is
    /// derived from it again").
    fn forgotten_removed(&mut self) {
        let dirs = &self.config.dirs;
        let index = |id: &str| crate::volumes::index_dir(dirs, id);
        self.volumes
            .forgetting
            .retain(|(_, reading)| !reading.ended_within(std::time::Duration::ZERO));
        let kept: Vec<PathBuf> = (self.volumes.kept.iter().map(|k| index(&k.id)))
            .chain(self.volume_roots.keys().map(|id| index(id)))
            .chain(self.volumes.forgetting.iter().map(|(id, _)| index(id)))
            .collect();
        let Ok(entries) = std::fs::read_dir(&dirs.cache) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if e.file_name().to_string_lossy().starts_with("volume-") && !kept.contains(&path) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    /// What reads every kept volume cancelled: the engine is stopping.
    fn cancel_volumes(&self) {
        for v in self.volume_roots.values() {
            v.cancel.cancel();
        }
    }

    /// A client's `volumes`: `forget` taken back, a volume no client keeps
    /// let go of and its index removed; `keep` kept; `release`, and every
    /// kept volume mounted at or below `release_at`, let be until it is
    /// seen unmounted, or a while; then followed again, and the client
    /// answered, with the reads of what was let go of still to end.
    fn volumes_asked(
        &mut self,
        client: &str,
        keep: &[String],
        forget: &[String],
        release: &[String],
        release_at: &[PathBuf],
    ) -> Result<Released, String> {
        if self.config.system {
            return Err("the system service keeps no removable volume".into());
        }
        // Listed once, before anything is changed: a listing that fails
        // fails the request, and what it releases at a point is what is
        // mounted there now.
        self.volumes.seen = skwird_crawl::removable::mounted()?;
        if crate::volumes::changed(&mut self.volumes.kept, client, keep, forget) {
            save_registered(&self.config.dirs, crate::volumes::FILE, &self.volumes.kept);
        }
        // Its index is removed once no read of it runs: one may save it.
        let forgotten: Vec<&String> = forget
            .iter()
            .filter(|id| !crate::volumes::is_kept(&self.volumes.kept, id))
            .collect();
        for id in &forgotten {
            self.let_go_of_kept(id);
        }
        let until = Instant::now() + crate::volumes::RELEASED_FOR;
        // `release_at` is resolved as the serving thread had it
        // (`skwird_crawl::canonical`), mount points as the system says them.
        let below = |at: &Path| release_at.iter().any(|p| at.starts_with(p));
        // A twin is read by none: releasing its mount point lets nothing go
        // of, and the root of its identity, mounted elsewhere, stays one.
        let twin = |id: &str, at: &Path| {
            let plan = &self.volumes.plan;
            plan.twins.iter().any(|(t, p)| t == id && p == at)
        };
        let at_points: Vec<String> = self
            .volumes
            .seen
            .iter()
            .filter(|s| below(&s.point))
            .filter_map(|s| s.id.clone().filter(|id| !twin(id, &s.point)))
            .chain(
                self.volume_roots
                    .iter()
                    .filter(|(_, v)| below(&v.at))
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        for id in release.iter().chain(&at_points) {
            if crate::volumes::is_kept(&self.volumes.kept, id) {
                self.volumes.released.insert(id.clone(), until);
            }
        }
        self.follow_seen();
        // The reads this answer waits for: of what it released or forgot.
        // What was let go of otherwise (unmounted, removed) is none of its.
        let ours = |id: &str| {
            let mut asked = release.iter().chain(&at_points).chain(forget);
            asked.any(|a| a == id)
        };
        let reading: Vec<(String, Arc<crate::volumes::Reading>)> =
            std::mem::take(&mut self.volumes.let_go)
                .into_iter()
                .filter(|(id, _)| ours(id))
                .collect();
        for (id, r) in &reading {
            if forgotten.contains(&id) {
                self.volumes.forgetting.push((id.clone(), Arc::clone(r)));
            }
        }
        self.forgotten_removed();
        Ok(Released {
            volumes: crate::volumes::answer(
                &self.volumes.kept,
                client,
                &self.volumes.seen,
                &self.volumes.plan,
                &self.volumes.released,
                Instant::now(),
            ),
            reading: reading.into_iter().map(|(_, r)| r).collect(),
        })
    }

    /// Root `i`'s tree watched again where its watch ended (Windows): since
    /// when, if it is. What its journal says proves nothing until it is
    /// checked.
    #[cfg(windows)]
    fn rewatch(&mut self, i: usize) -> Option<Time> {
        if self.marked.get(i).copied().flatten().is_some() {
            return None;
        }
        let watcher = self.watcher.as_ref()?;
        let path = self.keepers.get(i)?.as_ref()?.root().path().to_path_buf();
        let since = Time::now();
        self.journals.doubt(i);
        self.marked[i] = watch_whole(watcher, i, &path, self.config.system, None).0;
        self.hold_volume(i, &path);
        self.marked[i].map(|_| since)
    }

    /// Root `i` watched again where its watch ended (macOS): since when, if
    /// it is.
    #[cfg(target_os = "macos")]
    fn rewatch(&mut self, i: usize) -> Option<Time> {
        if self.marked.get(i).copied().flatten().is_some() {
            return None;
        }
        let watcher = self.watcher.as_ref()?;
        let path = self.keepers.get(i)?.as_ref()?.root().path().to_path_buf();
        let since = Time::now();
        self.journals.doubt(i);
        self.marked[i] = watch_whole(watcher, i, &path, None).0;
        self.marked[i].map(|_| since)
    }
}

/// Root `i`, at `root`, heard whole from now on and resumed from its
/// position kept, as each system does: marked, and whether it resumed.
#[cfg(windows)]
fn follow(config: &Config, w: &Watcher, i: usize, root: &Path) -> (Option<(u32, u32)>, bool) {
    watch_whole(w, i, root, config.system, kept_position(config, root))
}

#[cfg(target_os = "macos")]
fn follow(config: &Config, w: &Watcher, i: usize, root: &Path) -> (Option<(u32, u32)>, bool) {
    watch_whole(w, i, root, kept_position(config, root))
}

/// Root `i`, at `root`, heard whole from now on (macOS): its tree in the
/// roots' stream, and its history replayed from the position `kept` where
/// it still holds it (DESIGN 2). Said marked, as a filesystem is on Linux,
/// with no device to name; none if it cannot be, and it is checked, never
/// live. And whether it resumed from `kept`.
#[cfg(target_os = "macos")]
fn watch_whole(
    watcher: &Watcher,
    i: usize,
    root: &Path,
    kept: Option<Kept>,
) -> (Option<(u32, u32)>, bool) {
    if let Err(e) = watcher.watch_root(i, root) {
        log(&format!(
            "root {i} cannot be watched, so it is checked, never live: {e}"
        ));
        return (None, false);
    }
    let resumed = kept.is_some_and(|at| match watcher.replay_root(i, root, at) {
        Ok(true) => true,
        Ok(false) => {
            log(&format!(
                "root {i}: its history no longer holds its position kept, so it is checked"
            ));
            false
        }
        Err(e) => {
            log(&format!(
                "root {i}: its history cannot be replayed, so it is checked: {e}"
            ));
            false
        }
    });
    (Some((0, 0)), resumed)
}

/// The position root `root`'s history was heard up to, as its index kept
/// it at its last save (macOS): a user's engine's too, whose events the
/// user may replay. None where none was kept.
#[cfg(target_os = "macos")]
fn kept_position(config: &Config, root: &Path) -> Option<Kept> {
    let p = skwird_index::store::read_mark(&index_dir(&config.dirs, root))?.position?;
    Some(Kept {
        database: p.history,
        id: u64::try_from(p.next).ok()?,
    })
}

/// A client's registrations changed: `add` taken, `remove` let go of if it
/// was the client's. Says whether any changed.
fn changed(
    all: &mut Vec<Registration>,
    client: &str,
    uid: Option<u32>,
    add: &[PathBuf],
    remove: &[PathBuf],
) -> bool {
    // What is taken back first, then what is added, after what is kept:
    // a client puts a folder ahead of others by taking them back and
    // adding them again after it.
    let before = all.len();
    all.retain(|r| {
        !(r.client == client && r.uid == uid && remove.iter().any(|p| WirePath::of(p) == r.path))
    });
    let mut changed = all.len() != before;
    for path in add.iter().filter(|p| p.is_absolute()) {
        let r = Registration {
            client: client.to_owned(),
            uid,
            path: WirePath::of(path),
        };
        if !all.contains(&r) {
            all.push(r);
            changed = true;
        }
    }
    changed
}

/// What `client` registered, for user `uid` at the system service.
fn theirs(all: &[Registration], client: &str, uid: Option<u32>) -> Vec<PathBuf> {
    all.iter()
        .filter(|r| r.client == client && r.uid == uid)
        .map(|r| r.path.to_path())
        .collect()
}

/// Root `i`'s index proven current (the check), what is stale read again:
/// for the system service on NTFS, each folder judged by its volume's
/// file table, read whole in seconds, where the check stats every folder
/// (Windows, `mft::Table::check`); by every folder's own stat otherwise,
/// or where the table cannot be read. A table is read after the root is
/// followed, so what changes after it is heard.
fn check_index(
    keeper: &mut Keeper,
    i: usize,
    system: bool,
    cancel: &Cancel,
) -> Result<skwird_index::Refreshed, skwird_crawl::Error> {
    #[cfg(windows)]
    if system && keeper.root().fs_name() == "NTFS" {
        let start = Instant::now();
        match skwird_crawl::journal::Volume::table_of(keeper.root().path()) {
            Ok(table) => {
                log(&format!(
                    "root {i}: its volume's file table read in {:.2?}, to check it by",
                    start.elapsed()
                ));
                return keeper.refresh_by(cancel, &|root, stored, _, cancel| {
                    table.check(root, stored, cancel)
                });
            }
            Err(e) => log(&format!(
                "root {i}: its volume's file table cannot be read, so every folder is checked: {e}"
            )),
        }
    }
    let _ = (i, system);
    keeper.refresh(cancel)
}

/// Check root `root`, save what changed, publish it: whether it was
/// proven current.
fn refresh(
    keeper: &mut Keeper,
    root: usize,
    system: bool,
    shared: &Shared,
    cancel: &Cancel,
) -> bool {
    let start = Instant::now();
    let current = match check_index(keeper, root, system, cancel) {
        Ok(done) => {
            if !done.records.is_empty() {
                log(&format!(
                    "root {root} refreshed in {:.2?}: {} folders checked, {} read again, {} crawled",
                    start.elapsed(),
                    done.checked,
                    done.reread,
                    done.crawled
                ));
                if keeper.save().is_err() {
                    log("a save failed: the index is kept in memory, the log on disk");
                }
            }
            true
        }
        Err(e) => {
            log(&format!("root {root} cannot be refreshed: {e}"));
            false
        }
    };
    shared.publish(root, Published::Ready(Arc::new(keeper.snapshot())));
    // What the check and the rereads freed, given back before the engine
    // waits again: an engine idle after a sync holds what one at rest
    // holds (DESIGN 10).
    crate::memory::give_back();
    current
}

/// Whether a user's engine hears each root whole, by one handle a root on
/// Windows (`ReadDirectoryChangesW`) and one stream for every root on
/// macOS (`FSEvents`): live from its crawl or check, with no folder kept
/// live alone and no share of watches. On Linux, inotify watches each
/// folder, and what a user's engine keeps live is what its clients
/// registered. The step that undid it on macOS was a check of Windows
/// alone: each check of the kind asks this.
const HEARD_WHOLE: bool = cfg!(any(windows, target_os = "macos"));

/// How long a sync waits for its cookie before the check serves it.
const BARRIER: std::time::Duration = std::time::Duration::from_secs(2);

/// A kept volume that is a root: its number, its mount point, what
/// cancels its reading, its own, and its reads in progress.
struct VolumeRoot {
    root: usize,
    at: PathBuf,
    cancel: Cancel,
    reading: Arc<crate::volumes::Reading>,
    /// Its removal heard as it is asked (Windows): ended off the writer's
    /// turn, as ending it waits for its call, which may wait for the
    /// writer.
    #[cfg(windows)]
    held: Option<skwird_crawl::RemovalHeard>,
}

/// A client's `volumes` answered: what it keeps after and what is
/// mounted, and the reads of the volumes let go of, which a release waits
/// to end before it is answered.
pub struct Released {
    pub volumes: skwird_protocol::Volumes,
    pub reading: Vec<Arc<crate::volumes::Reading>>,
}

/// A sync of a live folder, waiting for its cookie to be heard.
struct Waiting {
    cookie: u64,
    root: usize,
    replies: Vec<Sender<()>>,
    until: Instant,
}

/// The folder the engine makes its cookies in, in its runtime folder,
/// watched by `watcher` (open question 1): the same queue as every
/// folder it watches. Cookies left by an engine before are let go of.
/// None on Windows, whose cookies are no files (`Writing::make_cookie`).
#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)] // The same call as elsewhere.
fn cookie_folder(_: &Dirs, _: &Watcher) -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn cookie_folder(dirs: &Dirs, watcher: &Watcher) -> Option<PathBuf> {
    let dir = dirs.runtime.join("cookies");
    let made = crate::dirs::make_private(&dir).and_then(|()| {
        for left in std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let _ = std::fs::remove_file(left.path());
        }
        watcher.watch_cookies(&dir).map_err(|e| e.to_string())
    });
    match made {
        Ok(()) => Some(dir),
        Err(e) => {
            log(&format!("no cookies, so a sync checks: {e}"));
            None
        }
    }
}

impl Writing<'_> {
    /// When something is due next: a gathering, a check, a save, or a
    /// sync whose cookie is late.
    fn deadline(&self) -> Option<Instant> {
        let late = self.waiting.iter().map(|w| w.until).min();
        // A volume released, and still mounted, is a root again then.
        let released = self.volumes.released.values().min().copied();
        [self.gather.deadline(), late, released]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether answers at `rel` below root `root` are live, and a sync of
    /// it a barrier: a cookie is heard in its turn after its changes.
    fn is_live(&self, root: usize, rel: &Path) -> bool {
        self.keepers[root]
            .as_ref()
            .is_some_and(|k| k.index().live_since(rel).is_some())
    }

    /// A cookie for a sync of root `root` (Windows): asked of the root's
    /// own thread, which says it once every change below the root before it
    /// is read (`Watcher::cookie`), with no file made. Each root is heard
    /// on a stream of its own there: a file made in the cookies' folder was
    /// heard by the root holding it alone, and a sync of any other root
    /// checked it whole. None where the root's thread cannot say one.
    #[cfg(windows)]
    fn make_cookie(&mut self, root: usize) -> Option<u64> {
        let n = self.next_cookie;
        let asked = self.watcher.as_ref().is_some_and(|w| w.cookie(root, n));
        if asked {
            self.next_cookie += 1;
        }
        asked.then_some(n)
    }

    /// A cookie made, its number given; none without a cookie folder. On
    /// macOS, whose events name a folder, not an entry, a cookie is a
    /// folder of its own with a file made in it, an event that names the
    /// cookie, heard within the stream's latency (`skwird_crawl`'s
    /// `LATENCY`, 50 ms). One cookie serves every root, all heard on one
    /// queue.
    #[cfg(not(windows))]
    fn make_cookie(&mut self, _root: usize) -> Option<u64> {
        let dir = self.cookies.as_ref()?;
        let n = self.next_cookie;
        self.next_cookie += 1;
        #[cfg(target_os = "macos")]
        let made = {
            let folder = skwird_crawl::cookie_folder(dir, n);
            std::fs::create_dir(&folder)
                .and_then(|()| std::fs::File::create(folder.join("made")))
                .map(drop)
        };
        #[cfg(not(target_os = "macos"))]
        let made = std::fs::File::create(dir.join(format!("cookie-{n}"))).map(drop);
        match made {
            Ok(()) => Some(n),
            Err(e) => {
                log(&format!("a cookie cannot be made, so a sync checks: {e}"));
                None
            }
        }
    }

    fn forget_cookie(&self, n: u64) {
        if let Some(dir) = &self.cookies {
            #[cfg(target_os = "macos")]
            let _ = std::fs::remove_dir_all(skwird_crawl::cookie_folder(dir, n));
            #[cfg(not(target_os = "macos"))]
            let _ = std::fs::remove_file(dir.join(format!("cookie-{n}")));
        }
    }

    /// The cookie numbered `n` heard: every event queued before it has
    /// been read and gathered. Each sync waiting for it or one before it
    /// is served: its root's gatherings applied, its answer sent.
    fn cookie_heard(&mut self, n: u64) {
        let (served, waiting): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| w.cookie <= n);
        self.waiting = waiting;
        self.served.extend(served);
    }

    /// The syncs whose cookie was heard, served: what was gathered of their
    /// root applied at once, then answered. Served after what was heard is
    /// found, outside the time charged to finding it: a client asked, and
    /// a sync is not charged (DESIGN 8). Counted apart, in the save line.
    fn serve_heard(&mut self) {
        if self.served.is_empty() {
            return;
        }
        let start = thread_time();
        for w in std::mem::take(&mut self.served) {
            self.forget_cookie(w.cookie);
            let heard = self.gather.take(w.root);
            self.apply_heard(w.root, &heard);
            for reply in w.replies {
                let _ = reply.send(());
            }
        }
        let took = thread_time().saturating_sub(start);
        self.done.syncing += took;
        self.for_clients += took;
    }

    /// A sync served by the check: what was heard applied first, then
    /// every folder of the root compared. A root being opened or checked
    /// meanwhile (a barrier given up late) is synced once it is, as a sync
    /// asked of it then.
    fn check_now(&mut self, root: usize, replies: Vec<Sender<()>>) {
        if self.opening.contains_key(&root) {
            for reply in replies {
                self.parked.push(Command::Sync {
                    root,
                    rel: PathBuf::new(),
                    reply,
                });
            }
            return;
        }
        let heard = self.gather.take(root);
        self.apply_heard(root, &heard);
        self.check_root_for(root, replies);
        self.gather.checked(Instant::now());
    }
}

impl Writing<'_> {
    /// The mount table changed: each root reads it again, and the folders
    /// at mount points made or taken below it are read again at once.
    fn mounts_changed(&mut self) {
        self.unfound.clear();
        self.marks_followed();
        self.roots_followed();
        self.volumes_followed();
        // On Windows, the registrations of volumes let go of ended, out of
        // the manager's call that asked; and a root whose volume is back
        // checked, and followed again: one let go of as its volume was taken
        // away, or whose watch ended.
        #[cfg(windows)]
        self.journals.held.retain(|_, held| !held.released());
        #[cfg(windows)]
        for i in 0..self.keepers.len() {
            let unheard = self.marked.get(i).copied().flatten().is_none();
            let there = self.keepers[i]
                .as_ref()
                .is_some_and(|k| k.root().path().exists());
            if self.config.system && unheard && there {
                self.check_root(i);
            } else if self.config.system && !unheard && !there && self.keepers[i].is_some() {
                // Gone with no word of it on its handle: let go of all the
                // same.
                self.let_go_of(i, "is gone");
            }
        }
        for root in 0..self.keepers.len() {
            let Some(keeper) = self.keepers[root].as_mut() else {
                continue;
            };
            match keeper.mounts_changed(&self.cancel) {
                Ok(done) if done.reread > 0 => {
                    log(&format!(
                        "root {root}: the mounts changed, {} folders read again",
                        done.reread
                    ));
                    self.gather.changed(Instant::now());
                }
                Ok(_) => {}
                Err(e) => log(&format!("root {root}: its mounts cannot be read: {e}")),
            }
            self.publish(root);
        }
    }
}

/// Root `i`, at `root`, heard whole from now on (Windows): for the system
/// service, through its volume's change journal (NTFS's), from the
/// position `kept` where the journal still holds it; otherwise, or where
/// its volume has none to follow (FAT, exFAT), its tree watched by one
/// handle. Said marked, as a filesystem is on Linux, with no device to
/// name; none if it cannot be, and it is checked, never live. And whether
/// it resumed from `kept`.
#[cfg(windows)]
fn watch_whole(
    watcher: &Watcher,
    i: usize,
    root: &Path,
    system: bool,
    kept: Option<skwird_crawl::journal::Position>,
) -> (Option<(u32, u32)>, bool) {
    if system {
        match watcher.follow_root_from(i, root, kept) {
            Ok(resumed) => return (Some((0, 0)), resumed),
            Err(e) => log(&format!(
                "root {i}: its volume's journal cannot be followed, so its tree is watched: {e}"
            )),
        }
    }
    match watcher.watch_root(i, root) {
        Ok(()) => (Some((0, 0)), false),
        Err(e) => {
            log(&format!(
                "root {i} cannot be watched, so it is checked, never live: {e}"
            ));
            (None, false)
        }
    }
}

/// The position root `root`'s journal was read up to, as its index kept
/// it at its last save (Windows' system service): none for a user's
/// engine, or where none was kept.
#[cfg(windows)]
fn kept_position(config: &Config, root: &Path) -> Option<skwird_crawl::journal::Position> {
    if !config.system {
        return None;
    }
    let p = skwird_index::store::read_mark(&index_dir(&config.dirs, root))?.position?;
    Some(skwird_crawl::journal::Position {
        journal: p.history,
        next: p.next,
    })
}

/// Mark root `i`'s filesystem, at `root`: its device, or none if it was
/// refused, said why, and the root is checked, never live.
fn mark(group: &Group, i: usize, root: &Path, table: &Table) -> Option<(u32, u32)> {
    match group.mark(root, table) {
        Ok(m) => {
            if m.new {
                log(&format!(
                    "root {i}: its filesystem marked{}",
                    match m.through {
                        fanotify::Through::Itself => "",
                        fanotify::Through::TopLevel => ", through its top-level subvolume",
                    }
                ));
            }
            Some(m.dev)
        }
        Err(why) => {
            log(&format!(
                "root {i}: its filesystem cannot be marked, so it is checked, never live: {why:?}"
            ));
            None
        }
    }
}

/// The system service's cookies' folder, in its state folder, which must
/// be on a filesystem marked: the same queue as every event (open question
/// 1). Cookies left by an engine before are let go of.
fn marked_cookie_folder(dirs: &Dirs, group: &Group, table: &Table) -> Option<PathBuf> {
    let dir = dirs.state.join("cookies");
    let made = crate::dirs::make_private(&dir).and_then(|()| {
        for left in std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let _ = std::fs::remove_file(left.path());
        }
        group.watch_cookies(&dir, table).map_err(|e| e.to_string())
    });
    match made {
        Ok(()) => Some(dir),
        Err(e) => {
            log(&format!("no cookies, so a sync checks: {e}"));
            None
        }
    }
}

impl Writing<'_> {
    /// Root `i`, at `path`, its filesystem marked before `since`, opened
    /// on a thread of its own: its index read back and checked, or
    /// crawled, so that the writer answers meanwhile and no stat of it is
    /// the writer's (AGENTS.md, "Blocking"). It comes back as
    /// [`Command::Opened`].
    fn open_later(&mut self, i: usize, path: PathBuf, since: Time, ex: Exclusions) {
        #[cfg(windows)]
        self.hold_volume(i, &path);
        // A kept volume: its index by its identity, its reading cancelled
        // apart, read at its pace (`crate::volumes`).
        let (dir, cancel, read) = match self.volume_roots.iter().find(|(_, v)| v.root == i) {
            Some((id, v)) => (
                crate::volumes::index_dir(&self.config.dirs, id),
                v.cancel.clone(),
                Some(v.reading.begin()),
            ),
            None => (
                index_dir(&self.config.dirs, &path),
                self.cancel.clone(),
                None,
            ),
        };
        let paced = read.is_some();
        self.openings += 1;
        let generation = self.openings;
        self.opening.insert(
            i,
            Opening {
                since,
                overflowed: false,
                registered: self.registered,
                generation,
                check: None,
            },
        );
        let (config, shared) = (self.config.clone(), Arc::clone(&self.shared_arc));
        let (mut opts, sender) = (self.opts.clone(), self.sender.clone());
        opts.background = paced;
        // Heard whole once marked (the system service): an index read back
        // is checked first. A user's engine hears its scopes alone.
        let marked = self.marked.get(i).copied().flatten().is_some();
        // One resumed from its journal's position kept is current once its
        // replay ends, with no check (Windows, DESIGN 2).
        #[cfg(any(windows, target_os = "macos"))]
        let resumed = self.journals.replaying.contains_key(&i);
        #[cfg(not(any(windows, target_os = "macos")))]
        let resumed = false;
        // A read of a kept volume, begun before this thread was made, is
        // ended before what it read is sent: the writer, taking it in, knows
        // it ended (`Writing::forgotten_removed`).
        crate::serve::named("skwird-open", move || {
            if paced {
                pause(crate::volumes::PAUSE, &cancel);
                // Released meanwhile: nothing of it is opened.
                if cancel.is_cancelled() {
                    drop(read);
                    let _ = sender.send(Command::Opened {
                        root: i,
                        generation,
                        keeper: None,
                        built: false,
                        current: false,
                    });
                    return;
                }
            }
            let (mut keeper, built) =
                open_one(&config, &shared, i, &path, &dir, &ex, &opts, &cancel);
            // A crawl proves it current; an index read back, its check.
            let mut current = built;
            if let Some(keeper) = keeper.as_mut().filter(|_| !built && marked && !resumed) {
                let start = Instant::now();
                match check_index(keeper, i, config.system, &cancel) {
                    Ok(done) => {
                        log(&format!(
                            "root {i} checked in {:.2?}: {} read again, {} crawled",
                            start.elapsed(),
                            done.reread,
                            done.crawled
                        ));
                        current = true;
                    }
                    Err(e) => log(&format!("root {i} cannot be checked, so not live: {e}")),
                }
            }
            drop(read);
            let _ = sender.send(Command::Opened {
                root: i,
                generation,
                keeper: keeper.map(Box::new),
                built,
                current,
            });
        });
    }

    /// Kept volume root `i` checked on a thread of its own, with its
    /// volume's cancel and a read of it, as it is opened (AGENTS.md,
    /// "Blocking"): a release cancels the check and waits for it a while,
    /// never for the writer, which answers meanwhile. Its keeper is the
    /// thread's until it comes back as [`Command::Opened`]: what is heard of
    /// it meanwhile is held, its syncs wait, and `replies` are answered
    /// then. Begun at `from`, its tree watched again first from `rewatched`,
    /// with its volume's cancel and the read of it begun.
    fn check_later(
        &mut self,
        i: usize,
        (cancel, read): (Cancel, crate::volumes::Read),
        rewatched: Option<Time>,
        from: Time,
        replies: Vec<Sender<()>>,
    ) {
        let Some(mut keeper) = self.keepers[i].take() else {
            answered(replies);
            return;
        };
        self.openings += 1;
        let generation = self.openings;
        self.opening.insert(
            i,
            Opening {
                since: from,
                overflowed: false,
                registered: self.registered,
                generation,
                check: Some(Check { rewatched, replies }),
            },
        );
        let (system, shared, sender) = (
            self.config.system,
            Arc::clone(&self.shared_arc),
            self.sender.clone(),
        );
        crate::serve::named("skwird-check", move || {
            let current = refresh(&mut keeper, i, system, &shared, &cancel);
            drop(read);
            let _ = sender.send(Command::Opened {
                root: i,
                generation,
                keeper: Some(Box::new(keeper)),
                built: false,
                current,
            });
        });
    }

    /// Root `root` opened ([`Writing::open_later`]): its keeper taken in,
    /// heard whole from its mark on where it was proven current, what was
    /// held for it handled, its exclusions derived again (a registration
    /// made meanwhile holds for it too), and the syncs that waited for it
    /// answered. Where what was held for it overflowed, it is checked
    /// before it is live, as after events dropped. One let go of meanwhile
    /// is let go of, and so is what a thread of an opening let go of gives
    /// back late (`generation`), the same root found again meanwhile. A
    /// kept volume's check ([`Writing::check_later`]) is taken in so too,
    /// as [`Writing::check_root`] takes in its own, and its syncs answered.
    fn opened(
        &mut self,
        root: usize,
        generation: u64,
        keeper: Option<Box<Keeper>>,
        built: bool,
        current: bool,
    ) {
        // Its thread's read ended: an index forgotten may be removed now.
        if !self.volumes.forgetting.is_empty() {
            self.forgotten_removed();
        }
        let opening = match self.opening.entry(root) {
            std::collections::hash_map::Entry::Occupied(o) if o.get().generation == generation => {
                o.remove()
            }
            _ => return,
        };
        if !self.shared.is_alive(root) {
            answered(opening.check.map(|c| c.replies).unwrap_or_default());
            return;
        }
        self.keepers[root] = keeper.map(|k| *k);
        // What was found in no index may be in this one.
        self.unfound.clear();
        if built {
            crate::memory::give_back();
        }
        let replies = if let Some(check) = opening.check {
            self.checked(root, check.rewatched, opening.since, current);
            check.replies
        } else {
            let marked = self.marked.get(root).copied().flatten().is_some();
            if let Some(keeper) = self.keepers[root].as_mut() {
                if current && marked {
                    keeper.heard_whole(opening.since);
                }
                // Built or checked, what its journal says proves from now on;
                // not resumed, the position its index kept holds no more.
                #[cfg(any(windows, target_os = "macos"))]
                {
                    if current {
                        self.journals.unproven.remove(&root);
                    }
                    if !self.journals.replaying.contains_key(&root) {
                        keeper.set_position(None);
                    }
                }
            }
            Vec::new()
        };
        if opening.overflowed
            && let Some(keeper) = self.keepers[root].as_mut()
        {
            log(&format!(
                "root {root}: events dropped while it opened: not live until checked"
            ));
            keeper.overflowed();
            self.gather.overflowed(Instant::now());
        }
        self.publish(root);
        answered(replies);
        self.replay_held();
        self.serve_heard();
        if self.registered != opening.registered {
            self.exclusions_held();
        }
        self.apply_scopes();
        self.unpark(root);
    }

    /// The syncs that waited for root `root` to be opened, answered as any.
    fn unpark(&mut self, root: usize) {
        let (theirs, others): (Vec<Command>, Vec<Command>) = std::mem::take(&mut self.parked)
            .into_iter()
            .partition(|c| matches!(c, Command::Sync { root: r, .. } if *r == root));
        self.parked = others;
        self.pending.extend(theirs);
    }

    /// An event no open root could take, held while roots are opening:
    /// past [`HELD_MOST`], none is held, and every root being opened is
    /// checked before it is live.
    fn hold(&mut self, event: HeldEvent) {
        if self.held.len() >= HELD_MOST {
            self.held.clear();
            self.opening_overflowed();
            return;
        }
        self.held.push(event);
    }

    /// Events were dropped while roots were opening: each is checked once
    /// opened, before it is live.
    fn opening_overflowed(&mut self) {
        for opening in self.opening.values_mut() {
            opening.overflowed = true;
        }
    }

    /// What was held handled again, a root having opened: what is still
    /// for a root opening is held again.
    fn replay_held(&mut self) {
        for event in std::mem::take(&mut self.held) {
            match event {
                HeldEvent::Marks(generation, heard) => self.noticed(generation, vec![heard]),
                HeldEvent::Watched(generation, heard) => self.heard(generation, vec![heard]),
            }
        }
    }

    /// The system service's roots as found now: its mount table's on Linux,
    /// read again; the machine's volumes on Windows.
    // Linux's keeps the table it read.
    #[cfg_attr(windows, allow(clippy::unused_self))]
    fn found_roots(&mut self) -> Vec<PathBuf> {
        #[cfg(windows)]
        return crate::system::volume_roots()
            .iter()
            .filter_map(|r| skwird_crawl::canonical(r).ok())
            .collect();
        #[cfg(not(windows))]
        {
            if self.group.is_none()
                && let Ok(table) = Table::read()
            {
                self.table = table;
            }
            crate::system::found(&self.table)
        }
    }

    /// A root found as the service runs, `i` at `path`, marked before it is
    /// read: its filesystem on Linux; on Windows, followed through its
    /// journal, proving nothing until it is built.
    fn mark_new(&mut self, i: usize, path: &Path) -> Option<(u32, u32)> {
        #[cfg(windows)]
        {
            self.journals.doubt(i);
            let watcher = self.watcher.as_ref()?;
            watch_whole(watcher, i, path, true, None).0
        }
        #[cfg(not(windows))]
        self.group
            .as_ref()
            .and_then(|g| mark(g, i, path, &self.table))
    }

    /// The system service's roots followed as filesystems are mounted and
    /// unmounted, where they are found in the mount table, not named: one
    /// mounted is marked, opened and heard as at the start; one unmounted
    /// is let go of, and a path there answered by the root holding it now.
    fn roots_followed(&mut self) {
        if !self.config.system || !self.config.discovered {
            return;
        }
        self.unfound.clear();
        let now = self.found_roots();
        let there = self.shared.roots();
        for path in there.iter().filter(|p| !now.contains(p)) {
            if let Some((i, rel)) = self.shared.locate(path)
                && rel.as_os_str().is_empty()
            {
                self.keepers[i] = None;
                self.marked[i] = None;
                #[cfg(windows)]
                {
                    if let Some(w) = &self.watcher {
                        w.unwatch_root(i);
                    }
                    self.journals.held.remove(&i);
                    self.journals.doubt(i);
                }
                self.shared.drop_root(i);
                // Being opened, it is let go of as it comes back; its syncs
                // are answered now, as of a root that is none.
                self.opening_given_up(i);
                log(&format!("root {i}: no longer mounted, let go of"));
            }
        }
        let added: Vec<&PathBuf> = now.iter().filter(|p| !there.contains(p)).collect();
        if added.is_empty() {
            return;
        }
        let (ex, refused) = exclusions(self.config, &self.registrations);
        log_refused(refused);
        for path in added {
            let i = self.shared.add_root(path);
            while self.keepers.len() <= i {
                self.keepers.push(None);
                self.marked.push(None);
            }
            log(&format!("root {i}: mounted, a root from now on"));
            self.marked[i] = self.mark_new(i, path);
            let since = Time::now();
            self.open_later(i, path.clone(), since, ex.clone());
        }
    }

    /// Each folder `heard` names found, before anything is applied: in the
    /// root holding it, or none; the filesystems a handle failed to open
    /// on; and what finding each took. One found before is taken while its
    /// root's index has the shape it had then, no folder made, moved or let
    /// go of since, and holds it with the identity it was found with
    /// ([`Keeper::holds`]); otherwise it is found again. One found in no
    /// index, in a root being opened, or gone, is taken so while that holds
    /// ([`Unfound`]); one whose handle failed to open is found again, its
    /// filesystem's coverage lowered. What finding
    /// takes is charged to the folder, as its application is (DESIGN 8).
    /// And the folders in a root being opened, whose events are held until
    /// it is.
    #[allow(clippy::type_complexity)] // three results of one pass, used once
    fn find_heard(
        &mut self,
        group: &Group,
        heard: &[fanotify::Heard],
    ) -> (
        Vec<Where>,
        BTreeSet<u64>,
        Vec<(usize, FolderId, std::time::Duration)>,
    ) {
        // Each folder of the batch found once, by its handle as the batch
        // holds it, not copied: what each event costs is one look in it
        // (DESIGN 8, "An event costs one look").
        let mut found: HashMap<&fanotify::Folder, Where> = HashMap::new();
        let mut wheres = Vec::with_capacity(heard.len());
        let mut failed: BTreeSet<u64> = BTreeSet::new();
        let mut charges = Vec::new();
        // Out of the writer for the batch, that the resolver may hold it as
        // what is found is noted: put back after.
        let table = std::mem::take(&mut self.table);
        let mut resolver = group.resolver(&table);
        // Taken before any is found: a tree made meanwhile on an opening
        // thread moves it, and what is found in none is found again.
        let shapes = skwird_index::tree::shapes();
        for h in heard {
            let (fanotify::Heard::Entry(folder, _) | fanotify::Heard::Changed(folder)) = h else {
                wheres.push(Where::Nowhere);
                continue;
            };
            if let Some(&at) = found.get(folder) {
                wheres.push(at);
                continue;
            }
            if let Some(&Found {
                root,
                folder: f,
                id,
                shape,
            }) = self.found.get(folder)
                && self
                    .keepers
                    .get(root)
                    .and_then(Option::as_ref)
                    .is_some_and(|k| k.holds(f, id, shape))
            {
                self.done.found_before += 1;
                found.insert(folder, Where::At(root, f));
                wheres.push(Where::At(root, f));
                continue;
            }
            if let Some(is_held) = self.known_unfound(folder, shapes) {
                self.done.known_before += 1;
                let at = if is_held { Where::Held } else { Where::Nowhere };
                found.insert(folder, at);
                wheres.push(at);
                continue;
            }
            let start = thread_time();
            let opened = resolver.resolve(folder);
            let (at, kept) = self.placed(opened, folder.fsid, shapes, &mut failed);
            let at = if let Some(at) = at {
                if self.found.len() >= FOUND_KEPT {
                    self.found.clear();
                }
                self.found.insert(folder.clone(), at);
                self.done.found_again += 1;
                charges.push((at.root, at.folder, thread_time().saturating_sub(start)));
                Where::At(at.root, at.folder)
            } else {
                let held = matches!(kept, Some(Unfound::Held(_)));
                if let Some(kept) = kept {
                    if self.unfound.len() >= FOUND_KEPT {
                        self.unfound.clear();
                    }
                    self.unfound.insert(folder.clone(), kept);
                }
                if held { Where::Held } else { Where::Nowhere }
            };
            found.insert(folder, at);
            wheres.push(at);
        }
        // Linux's resolver lets go of the mounts it opened; elsewhere, a
        // stub with nothing to let go of (rule 7).
        #[cfg_attr(not(target_os = "linux"), allow(clippy::drop_non_drop))]
        drop(resolver);
        self.table = table;
        (wheres, failed, charges)
    }

    /// Where the folder whose handle opened to `resolved` is in an index,
    /// if it is, and what is kept of it found in none
    /// ([`Writing::find_heard`]). One whose handle failed to open lowers its
    /// filesystem's coverage, its id put in `failed`.
    fn placed(
        &mut self,
        resolved: Resolved,
        fsid: u64,
        shapes: u64,
        failed: &mut BTreeSet<u64>,
    ) -> (Option<Found>, Option<Unfound>) {
        // What is kept of it found in no index, if anything.
        let mut kept = None;
        let at = match resolved {
            Resolved::At(path, id) => match self.shared.locate(&path) {
                // In a root being opened: held until it is.
                Some((root, _)) if self.opening_root(root) => {
                    self.done.held += 1;
                    kept = Some(Unfound::Held(root));
                    None
                }
                Some((root, rel)) => {
                    let at = self.keepers[root].as_ref().and_then(|keeper| {
                        keeper.folder_of(&rel, id).map(|f| Found {
                            root,
                            folder: f,
                            id,
                            shape: keeper.shape(),
                        })
                    });
                    if at.is_none() {
                        self.done.nowhere += 1;
                        kept = Some(Unfound::Nowhere(shapes));
                    }
                    at
                }
                None => {
                    self.done.nowhere += 1;
                    kept = Some(Unfound::Nowhere(shapes));
                    None
                }
            },
            // Gone, its removal is heard in a parent: its handle never
            // opens again.
            Resolved::Gone => {
                self.done.gone += 1;
                kept = Some(Unfound::Gone);
                None
            }
            // No mount reaches it, or no mark kept has it: no root holds
            // it, until a mount or an index changes (a folder renamed as
            // it was opened is reached by none, and changes its index).
            Resolved::Unreachable(_) | Resolved::Unmarked => {
                self.done.unreached += 1;
                kept = Some(Unfound::Nowhere(shapes));
                None
            }
            // Not kept: it lowers the coverage of its filesystem.
            Resolved::Failed(_) => {
                self.done.failed += 1;
                failed.insert(fsid);
                None
            }
        };
        (at, kept)
    }

    /// Whether root `root` is being opened, its events held.
    fn opening_root(&self, root: usize) -> bool {
        self.keepers.get(root).is_some_and(Option::is_none) && self.opening.contains_key(&root)
    }

    /// What is kept of `folder`, found before in no index, in a root being
    /// opened, or gone, if it still holds ([`Unfound`]): whether it is held.
    /// None where nothing kept holds.
    fn known_unfound(&self, folder: &fanotify::Folder, shapes: u64) -> Option<bool> {
        match *self.unfound.get(folder)? {
            Unfound::Nowhere(at) if at == shapes => Some(false),
            Unfound::Held(root) if self.opening_root(root) => Some(true),
            Unfound::Gone => Some(false),
            _ => None,
        }
    }

    /// What the group heard: each folder named found by its handle, at its
    /// path, in the root holding it, known by its identity, and gathered;
    /// an overflow, or a handle that fails to open, lowering coverage at
    /// once for what it concerns, the check due once the queue is quiet.
    fn noticed(&mut self, generation: u64, heard: Vec<fanotify::Heard>) {
        let Some(group) = self.group.clone() else {
            return;
        };
        let now = Instant::now();
        self.done.events += heard.len() as u64;
        let (wheres, failed, charges) = self.find_heard(&group, &heard);
        let mut lowered = BTreeSet::new();
        // Each event with where its folder was found, in step.
        for (h, at) in heard.into_iter().zip(wheres) {
            match (h, at) {
                (h @ (fanotify::Heard::Entry(..) | fanotify::Heard::Changed(_)), Where::Held) => {
                    self.hold(HeldEvent::Marks(generation, h));
                }
                (fanotify::Heard::Entry(_, name), Where::At(root, f)) => {
                    self.gather.note_entry(root, f, name, now);
                }
                (fanotify::Heard::Changed(_), Where::At(root, f)) => {
                    self.gather.note(root, f, now);
                }
                (fanotify::Heard::Entry(..) | fanotify::Heard::Changed(_), Where::Nowhere) => {}
                // Only the current group's cookie proves its queue read.
                (fanotify::Heard::Cookie(n), _) => {
                    if group.generation() == generation {
                        self.cookie_heard(n);
                    }
                }
                (fanotify::Heard::Overflow, _) => {
                    self.done.overflows += 1;
                    self.opening_overflowed();
                    for (i, keeper) in self.keepers.iter_mut().enumerate() {
                        if let Some(keeper) = keeper {
                            keeper.overflowed();
                            lowered.insert(i);
                        }
                    }
                }
            }
        }
        for (root, f, took) in charges {
            self.gather.found(root, f, took);
        }
        // A handle that did not open proves nothing heard on its filesystem.
        let marks = group.marks();
        for fsid in failed {
            let Some(dev) = marks.iter().find(|(_, id)| *id == fsid).map(|(d, _)| *d) else {
                continue;
            };
            for (i, keeper) in self.keepers.iter_mut().enumerate() {
                if self.marked[i] == Some(dev)
                    && let Some(keeper) = keeper
                {
                    keeper.overflowed();
                    lowered.insert(i);
                }
            }
        }
        if !lowered.is_empty() {
            log("events dropped, or what was heard could not be opened: not live until checked");
            self.gather.overflowed(now);
            for i in lowered {
                self.publish(i);
            }
        }
    }

    /// The mount table changed, for the group: read again, so that handles
    /// are opened through the mounts there are now; a filesystem no longer
    /// mounted has lost its mark with its superblock, and its roots are not
    /// live again until the engine starts again.
    fn marks_followed(&mut self) {
        let Some(group) = self.group.clone() else {
            return;
        };
        match Table::read() {
            Ok(table) => self.table = table,
            Err(e) => log(&format!("cannot read the mount table again: {e}")),
        }
        for (dev, _) in group.marks() {
            if self.table.mounts().iter().any(|m| m.dev == dev) {
                continue;
            }
            group.forget(dev);
            for root in 0..self.keepers.len() {
                if self.marked[root] == Some(dev) {
                    self.marked[root] = None;
                    if let Some(keeper) = self.keepers[root].as_mut() {
                        keeper.unheard();
                    }
                    log(&format!(
                        "root {root}: its filesystem is no longer mounted: not live"
                    ));
                    self.publish(root);
                }
            }
        }
    }
}

/// What was done for what was heard, said at each save (counts alone, so
/// that the log can be shared): what a live folder costs, and where.
#[derive(Default)]
struct Done {
    events: u64,
    overflows: u64,
    reread: u64,
    /// Of the folders read again, how many were patched into their
    /// chunk, spliced into it, and cut it again
    /// (`skwird_index::update::Written`).
    patched: u64,
    spliced: u64,
    cut: u64,
    /// Folders new since, crawled with those read again.
    crawled: u64,
    entries: u64,
    largest: usize,
    /// How often each folder was read again.
    by_folder: std::collections::HashMap<(usize, FolderId), u32>,
    /// The writer's processor time for what was heard: finding the
    /// folders events name, applying them, saving, and giving memory back
    /// (the last two, of the save before).
    noticing: std::time::Duration,
    applying: std::time::Duration,
    saving: std::time::Duration,
    giving_back: std::time::Duration,
    /// Serving syncs: what was gathered of their root applied at once, or
    /// the check serving one whose cookie came late. Not charged.
    syncing: std::time::Duration,
    /// Of the folders events named, each batch's (the system service,
    /// `Writing::find_heard`): those found before, and those found again;
    /// those found in no index, in a root being opened, gone, reached by no
    /// mount, or whose handle failed to open; and those known so before.
    found_before: u64,
    found_again: u64,
    nowhere: u64,
    held: u64,
    gone: u64,
    unreached: u64,
    failed: u64,
    known_before: u64,
    /// How often the writer woke, and its whole processor time, since
    /// the last save: what the kinds above leave out is its own.
    wakes: u64,
    since: std::time::Duration,
    /// What each folder read again for what was heard cost, its share of
    /// its batch by entries: [`READ_SAMPLES`] at most.
    reads: Vec<std::time::Duration>,
}

/// The most reads' costs a save's line is told of.
const READ_SAMPLES: usize = 4096;

impl Done {
    /// The folders `read` again took `took`: each its share, by entries.
    #[allow(clippy::cast_precision_loss)] // a share, from counts far below 2^52
    fn reads_took(&mut self, read: &[(FolderId, usize)], took: std::time::Duration) {
        let total: usize = read.iter().map(|(_, n)| n + 1).sum();
        for &(_, n) in read {
            if self.reads.len() < READ_SAMPLES {
                self.reads
                    .push(took.mul_f64((n + 1) as f64 / total.max(1) as f64));
            }
        }
    }

    fn read_again(&mut self, root: usize, done: &skwird_index::Refreshed) {
        self.reread += done.reread as u64;
        self.patched += done.patched as u64;
        self.spliced += done.spliced as u64;
        self.cut += done.cut as u64;
        self.crawled += done.crawled as u64;
        self.entries += done.entries as u64;
        self.largest = self.largest.max(done.largest);
        for &(folder, _) in &done.folders {
            *self.by_folder.entry((root, folder)).or_default() += 1;
        }
    }

    /// Said, and begun again.
    fn say(&mut self) {
        let now = thread_time();
        let whole = now.saturating_sub(self.since);
        let done = std::mem::take(self);
        self.since = now;
        if done.events == 0 {
            return;
        }
        let busiest = done.by_folder.values().max().copied().unwrap_or(0);
        let mut reads = done.reads.clone();
        reads.sort_unstable();
        let read = |at: Option<&std::time::Duration>| at.map_or(0, std::time::Duration::as_micros);
        log(&format!(
            "since the last save: {} events heard, {} overflows; {} folders read again \
             ({} distinct; {} as they were, {} patched, {} spliced, {} cut again), {} folders crawled, {} \
             entries stat-ed, the largest folder {} entries, the busiest read {} times, a read {} µs \
             at the median, {} µs at most; of the folders events named, {} as found before, {} \
             found again, {} in no index, {} in a root being opened, {} gone, {} reached by \
             no mount, {} failed to open; {} known so before; processor time: {} ms \
             finding, {} ms applying, {} ms saving, {} ms giving memory back, {} ms serving syncs, of {} ms the \
             writer took, woken {} times",
            done.events,
            done.overflows,
            done.reread,
            done.by_folder.len(),
            done.reread
                .saturating_sub(done.patched + done.spliced + done.cut),
            done.patched,
            done.spliced,
            done.cut,
            done.crawled,
            done.entries,
            done.largest,
            busiest,
            read(reads.get(reads.len() / 2)),
            read(reads.last()),
            done.found_before,
            done.found_again,
            done.nowhere,
            done.held,
            done.gone,
            done.unreached,
            done.failed,
            done.known_before,
            done.noticing.as_millis(),
            done.applying.as_millis(),
            done.saving.as_millis(),
            done.giving_back.as_millis(),
            done.syncing.as_millis(),
            whole.as_millis(),
            done.wakes
        ));
    }
}

/// The processor time this thread has taken.
fn thread_time() -> std::time::Duration {
    crate::os::thread_time()
}
