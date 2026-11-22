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
use skwird_index::keeper::{Event, Keeper, Opened, Reconciled, TooOld};
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
        .map(crate::asker::owned_at)
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
        reply: Sender<Result<(Vec<Event>, Token), TooOld>>,
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
        reply: Sender<Vec<PathBuf>>,
    },
    /// The mount table changed ([`crate::live::listen`]).
    Mounts,
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

/// The most folders heard the system service keeps as found: past them,
/// every one is let go of, and found again when heard.
const FOUND_KEPT: usize = 4096;

/// The most folders a user keeps out at the system service, each derived
/// again at every sync.
const MOST_REGISTERED: usize = 256;

/// What clients registered, kept across restarts.
fn registered(dirs: &Dirs, file: &str) -> Vec<Registration> {
    std::fs::read(dirs.state.join(file))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_registered(dirs: &Dirs, file: &str, all: &[Registration]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dirs.state.join(file);
    let tmp = dirs.state.join(format!("{file}.tmp"));
    // Its user's alone (0600), whatever the umask: the system service takes
    // a user's file only if no one else may write it.
    let write = |bytes: &[u8]| {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
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
    // take another's folder out.
    let (homes, store) = if config.system {
        (Vec::new(), vec![PathBuf::from(crate::system::NIX_STORE)])
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
        let at_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.clone());
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
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
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

/// Where a root's index is kept: a folder named by its path's hash.
pub fn index_dir(dirs: &Dirs, root: &Path) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in root.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    dirs.cache.join(format!("index-{hash:016x}"))
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
            if !w.answer(commands, command) {
                break;
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
        w.gather.turned(
            end.saturating_sub(mark).saturating_sub(asked),
            Instant::now(),
        );
        mark = end;
    }
}

pub fn writer(
    config: &Config,
    shared: &Shared,
    commands: &Receiver<Command>,
    watcher: Option<Arc<Watcher>>,
    group: Option<Arc<Group>>,
) {
    let opts = Options {
        watcher: watcher.clone(),
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
    let marked: Vec<Option<(u32, u32)>> = match &group {
        Some(g) => config
            .roots
            .iter()
            .enumerate()
            .map(|(i, root)| mark(g, i, root, &table))
            .collect(),
        None => vec![None; config.roots.len()],
    };
    let since = Time::now();
    let cookies = match (&watcher, &group) {
        (Some(w), _) => cookie_folder(&config.dirs, w),
        (None, Some(g)) => marked_cookie_folder(&config.dirs, g, &table),
        (None, None) => None,
    };
    let (keepers, built) = open_all(config, shared, &registrations, &opts, &cancel);
    if built.contains(&true) {
        // What a first build freed, given back now that it is saved.
        crate::memory::give_back();
    }
    let mut w = Writing {
        config,
        shared,
        cancel,
        opts: opts.clone(),
        keepers,
        registrations,
        kept_live,
        watcher,
        group,
        table,
        marked,
        found: HashMap::new(),
        gather: Gather::default(),
        cookies,
        next_cookie: 0,
        waiting: Vec::new(),
        done: Done {
            since: thread_time(),
            ..Done::default()
        },
        pending: Vec::new(),
    };
    w.hear_whole(since, &built);
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
    let mut paths: Vec<PathBuf> = registrations
        .iter()
        .filter_map(|r| std::fs::canonicalize(r.path.to_path()).ok())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Open every root: its index read back, or crawled; each published as it
/// is ready. Says which were built.
fn open_all(
    config: &Config,
    shared: &Shared,
    registrations: &[Registration],
    opts: &Options,
    cancel: &Cancel,
) -> (Vec<Option<Keeper>>, Vec<bool>) {
    let (ex, refused) = exclusions(config, registrations);
    log_refused(refused);
    config
        .roots
        .iter()
        .enumerate()
        .map(|(i, path)| open_one(config, shared, i, path, &ex, opts, cancel))
        .unzip()
}

/// Open root `i`, at `path`: its index read back, or crawled; published
/// once ready. Says whether it was built.
fn open_one(
    config: &Config,
    shared: &Shared,
    i: usize,
    path: &Path,
    ex: &Exclusions,
    opts: &Options,
    cancel: &Cancel,
) -> (Option<Keeper>, bool) {
    let start = Instant::now();
    let opened = Root::open(path, ex.clone()).and_then(|root| {
        Keeper::open(
            &index_dir(&config.dirs, path),
            root,
            opts.clone(),
            Limits::default(),
            cancel,
        )
    });
    match opened {
        Ok((keeper, how)) => {
            let sizes = keeper.index().sizes();
            log(&format!(
                "root {i} {} in {:.2?}: {} entries in {} folders",
                match how {
                    Opened::Loaded { replayed, .. } =>
                        format!("loaded, {replayed} records replayed"),
                    Opened::Built { because: None } => "built".to_owned(),
                    Opened::Built { because: Some(_) } => "built again".to_owned(),
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
    /// What was heard and not applied yet, and what is due.
    gather: Gather,
    /// The folder cookies are made in, once watched.
    cookies: Option<PathBuf>,
    next_cookie: u64,
    /// Syncs waiting for their cookie.
    waiting: Vec<Waiting>,
    /// What was done for what was heard, since the last save.
    done: Done,
    /// Commands taken while gathering syncs, to answer next.
    pending: Vec<Command>,
}

impl Writing<'_> {
    /// Answer one command: false to stop.
    fn answer(&mut self, commands: &Receiver<Command>, command: Command) -> bool {
        match command {
            Command::Stop => return false,
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
                // served by the barrier or by the check (DESIGN 5).
                self.hold_to_exclusions();
                // Live, a barrier serves: a cookie heard in its turn says
                // every change before it was heard (DESIGN 2). Otherwise,
                // the check.
                if live && let Some(cookie) = self.make_cookie() {
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
                    (Some(keeper), _) if since.is_empty() => {
                        keeper.token().map(|t| (Vec::new(), t)).ok_or(TooOld)
                    }
                    (Some(keeper), Some(token)) => keeper.changes(&rel, token),
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
            Command::Watch {
                client,
                add,
                remove,
                reply,
            } => {
                if changed(&mut self.kept_live, &client, None, &add, &remove) {
                    save_registered(&self.config.dirs, WATCHED, &self.kept_live);
                    self.apply_scopes();
                }
                let _ = reply.send(theirs(&self.kept_live, &client, None));
            }
            // What finding what was heard takes is charged to the account
            // (`live::DUTY`), as applying it is.
            Command::Heard { generation, heard } => {
                let start = thread_time();
                self.heard(generation, heard);
                self.noticing(start);
            }
            Command::Marks { generation, heard } => {
                let start = thread_time();
                self.noticed(generation, heard);
                self.noticing(start);
            }
            Command::Mounts => self.mounts_changed(),
        }
        true
    }

    /// What the watcher heard: each folder named gathered, an overflow or
    /// a watched subtree's own folder lost lowering coverage at once, the
    /// check due once the queue is quiet.
    fn heard(&mut self, generation: u64, heard: Vec<Heard>) {
        let now = Instant::now();
        self.done.events += heard.len() as u64;
        // An older watcher's changes are applied all the same (a read is
        // never wrong), but its cookies prove nothing (DESIGN 2).
        let mut lowered = BTreeSet::new();
        for h in heard {
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
        // A cookie not heard in time: the barrier failed, the check serves.
        let (late, waiting): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| now >= w.until);
        self.waiting = waiting;
        for w in late {
            self.forget_cookie(w.cookie);
            self.check_now(w.root, w.replies);
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
        if self.gather.recheck_due(now) {
            let start = thread_time();
            for (i, keeper) in self.keepers.iter_mut().enumerate() {
                if let Some(keeper) = keeper {
                    refresh(keeper, i, self.shared, &self.cancel);
                }
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

    /// Every gathering applied at once, as the engine stops.
    fn flush_all(&mut self) {
        for root in 0..self.keepers.len() {
            let heard = self.gather.take(root);
            self.apply_heard(root, &heard);
        }
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
        let (readmitting, refused) = self.hold_to_exclusions();
        log_refused(refused);
        for i in readmitting {
            if let Some(keeper) = self.keepers[i].as_mut() {
                refresh(keeper, i, self.shared, &self.cancel);
            }
        }
    }
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
    let mut changed = false;
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
    let before = all.len();
    all.retain(|r| {
        !(r.client == client && r.uid == uid && remove.iter().any(|p| WirePath::of(p) == r.path))
    });
    changed | (all.len() != before)
}

/// What `client` registered, for user `uid` at the system service.
fn theirs(all: &[Registration], client: &str, uid: Option<u32>) -> Vec<PathBuf> {
    all.iter()
        .filter(|r| r.client == client && r.uid == uid)
        .map(|r| r.path.to_path())
        .collect()
}

fn refresh(keeper: &mut Keeper, root: usize, shared: &Shared, cancel: &Cancel) {
    let start = Instant::now();
    match keeper.refresh(cancel) {
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
        }
        Err(e) => log(&format!("root {root} cannot be refreshed: {e}")),
    }
    shared.publish(root, Published::Ready(Arc::new(keeper.snapshot())));
    // What the check and the rereads freed, given back before the engine
    // waits again: an engine idle after a sync holds what one at rest
    // holds (DESIGN 10).
    crate::memory::give_back();
}

/// How long a sync waits for its cookie before the check serves it.
const BARRIER: std::time::Duration = std::time::Duration::from_secs(2);

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
        [self.gather.deadline(), late].into_iter().flatten().min()
    }

    /// Whether answers at `rel` below root `root` are live.
    fn is_live(&self, root: usize, rel: &Path) -> bool {
        self.keepers[root]
            .as_ref()
            .is_some_and(|k| k.index().live_since(rel).is_some())
    }

    /// A cookie made, its number given; none without a cookie folder.
    fn make_cookie(&mut self) -> Option<u64> {
        let dir = self.cookies.as_ref()?;
        let n = self.next_cookie;
        self.next_cookie += 1;
        match std::fs::File::create(dir.join(format!("cookie-{n}"))) {
            Ok(_) => Some(n),
            Err(e) => {
                log(&format!("a cookie cannot be made, so a sync checks: {e}"));
                None
            }
        }
    }

    fn forget_cookie(&self, n: u64) {
        if let Some(dir) = &self.cookies {
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
        for w in served {
            self.forget_cookie(w.cookie);
            let heard = self.gather.take(w.root);
            self.apply_heard(w.root, &heard);
            for reply in w.replies {
                let _ = reply.send(());
            }
        }
    }

    /// A sync served by the check: what was heard applied first, then
    /// every folder of the root compared.
    fn check_now(&mut self, root: usize, replies: Vec<Sender<()>>) {
        let heard = self.gather.take(root);
        self.apply_heard(root, &heard);
        if let Some(keeper) = self.keepers[root].as_mut() {
            refresh(keeper, root, self.shared, &self.cancel);
        }
        self.gather.checked(Instant::now());
        for reply in replies {
            let _ = reply.send(());
        }
    }
}

impl Writing<'_> {
    /// The mount table changed: each root reads it again, and the folders
    /// at mount points made or taken below it are read again at once.
    fn mounts_changed(&mut self) {
        self.marks_followed();
        self.roots_followed();
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
    /// Every root whose filesystem was marked before `since` heard whole
    /// from then on: once its index is proven current after the mark, by
    /// its crawl (`built`) or by the check now.
    fn hear_whole(&mut self, since: Time, built: &[bool]) {
        for (root, &crawled) in built.iter().enumerate() {
            self.hear_root(root, since, crawled);
        }
    }

    /// Root `root`, its filesystem marked before `since`, heard whole from
    /// then on, once proven current after: by its crawl (`crawled`), or by
    /// the check now.
    fn hear_root(&mut self, root: usize, since: Time, crawled: bool) {
        if self.marked.get(root).copied().flatten().is_none() {
            return;
        }
        let Some(keeper) = self.keepers.get_mut(root).and_then(Option::as_mut) else {
            return;
        };
        if !crawled {
            let start = Instant::now();
            match keeper.refresh(&self.cancel) {
                Ok(done) => log(&format!(
                    "root {root} checked in {:.2?}: {} read again, {} crawled",
                    start.elapsed(),
                    done.reread,
                    done.crawled
                )),
                Err(e) => {
                    log(&format!("root {root} cannot be checked, so not live: {e}"));
                    return;
                }
            }
        }
        keeper.heard_whole(since);
        self.publish(root);
    }

    /// The system service's roots followed as filesystems are mounted and
    /// unmounted, where they are found in the mount table, not named: one
    /// mounted is marked, opened and heard as at the start; one unmounted
    /// is let go of, and a path there answered by the root holding it now.
    fn roots_followed(&mut self) {
        if !self.config.system || !self.config.discovered {
            return;
        }
        if self.group.is_none()
            && let Ok(table) = Table::read()
        {
            self.table = table;
        }
        let now = crate::system::roots(&self.table, crate::system::removable);
        let there = self.shared.roots();
        for path in there.iter().filter(|p| !now.contains(p)) {
            if let Some((i, rel)) = self.shared.locate(path)
                && rel.as_os_str().is_empty()
            {
                self.keepers[i] = None;
                self.marked[i] = None;
                self.shared.drop_root(i);
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
            self.marked[i] = self
                .group
                .as_ref()
                .and_then(|g| mark(g, i, path, &self.table));
            let since = Time::now();
            let (keeper, built) = open_one(
                self.config,
                self.shared,
                i,
                path,
                &ex,
                &self.opts,
                &self.cancel,
            );
            self.keepers[i] = keeper;
            self.hear_root(i, since, built);
        }
    }

    /// Each folder `heard` names found, before anything is applied: in the
    /// root holding it, or none; the filesystems a handle failed to open
    /// on; and what finding each took. One found before is taken while its
    /// root's index has the shape it had then, no folder made, moved or let
    /// go of since, and holds it with the identity it was found with
    /// ([`Keeper::holds`]); otherwise it is found again. What finding takes
    /// is charged to the folder, as its application is (DESIGN 8).
    #[allow(clippy::type_complexity)] // three results of one pass, used once
    fn find_heard(
        &mut self,
        group: &Group,
        heard: &[fanotify::Heard],
    ) -> (
        HashMap<fanotify::Folder, Option<(usize, FolderId)>>,
        BTreeSet<u64>,
        Vec<(usize, FolderId, std::time::Duration)>,
    ) {
        let mut found: HashMap<fanotify::Folder, Option<(usize, FolderId)>> = HashMap::new();
        let mut failed: BTreeSet<u64> = BTreeSet::new();
        let mut charges = Vec::new();
        let mut resolver = group.resolver(&self.table);
        for h in heard {
            let (fanotify::Heard::Entry(folder, _) | fanotify::Heard::Changed(folder)) = h else {
                continue;
            };
            if found.contains_key(folder) {
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
                found.insert(folder.clone(), Some((root, f)));
                continue;
            }
            let start = thread_time();
            let at = match resolver.resolve(folder) {
                Resolved::At(path, id) => self.shared.locate(&path).and_then(|(root, rel)| {
                    let keeper = self.keepers[root].as_ref()?;
                    keeper.folder_of(&rel, id).map(|f| Found {
                        root,
                        folder: f,
                        id,
                        shape: keeper.shape(),
                    })
                }),
                // Gone, its removal is heard in a parent; no mount reaches
                // it, or no mark kept has it: no root holds it.
                Resolved::Gone | Resolved::Unreachable(_) | Resolved::Unmarked => None,
                Resolved::Failed(_) => {
                    failed.insert(folder.fsid);
                    None
                }
            };
            if let Some(at) = at {
                if self.found.len() >= FOUND_KEPT {
                    self.found.clear();
                }
                self.found.insert(folder.clone(), at);
                charges.push((at.root, at.folder, thread_time().saturating_sub(start)));
            }
            found.insert(folder.clone(), at.map(|at| (at.root, at.folder)));
        }
        (found, failed, charges)
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
        let (found, failed, charges) = self.find_heard(&group, &heard);
        let mut lowered = BTreeSet::new();
        for h in heard {
            match h {
                fanotify::Heard::Entry(folder, name) => {
                    if let Some(&Some((root, f))) = found.get(&folder) {
                        self.gather.note_entry(root, f, name, now);
                    }
                }
                fanotify::Heard::Changed(folder) => {
                    if let Some(&Some((root, f))) = found.get(&folder) {
                        self.gather.note(root, f, now);
                    }
                }
                // Only the current group's cookie proves its queue read.
                fanotify::Heard::Cookie(n) => {
                    if group.generation() == generation {
                        self.cookie_heard(n);
                    }
                }
                fanotify::Heard::Overflow => {
                    self.done.overflows += 1;
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
             ({} distinct), {} entries stat-ed, the largest folder {} entries, the busiest \
             read {} times, a read {} µs at the median, {} µs at most; processor time: {} ms \
             finding, {} ms applying, {} ms saving, {} ms giving memory back, of {} ms the \
             writer took, woken {} times",
            done.events,
            done.overflows,
            done.reread,
            done.by_folder.len(),
            done.entries,
            done.largest,
            busiest,
            read(reads.get(reads.len() / 2)),
            read(reads.last()),
            done.noticing.as_millis(),
            done.applying.as_millis(),
            done.saving.as_millis(),
            done.giving_back.as_millis(),
            whole.as_millis(),
            done.wakes
        ));
    }
}

/// The processor time this thread has taken.
fn thread_time() -> std::time::Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    std::time::Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}
