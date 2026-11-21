//! The engine's writer: one thread owns each root's index and its log
//! (`skwird_index::keeper`), and is the only one to change them. Queries
//! read the snapshot it publishes, swapped whole: one writer, so no write
//! conflicts to resolve; queries hold no lock while they run.
//!
//! It opens each root (its index read back, or crawled), then waits for a
//! command: a sync (the check, run now; syncs asked meanwhile served by
//! the same refresh), the journal, a client's registrations. Nothing wakes
//! it otherwise: idle is idle.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use skwird_crawl::{Cancel, Exclusions, Heard, Options, Root, Watcher};
use skwird_index::Index;
use skwird_index::block::FolderId;
use skwird_index::keeper::{Event, Keeper, Opened, Reconciled, TooOld};
use skwird_index::log::Token;
use skwird_index::tree::Limits;
use skwird_protocol::WirePath;

use crate::dirs::Dirs;
use crate::live::Gather;

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
    pub roots: Vec<PathBuf>,
    pub published: RwLock<Vec<Published>>,
}

impl Shared {
    pub fn new(roots: Vec<PathBuf>) -> Shared {
        let n = roots.len();
        Shared {
            roots,
            published: RwLock::new(vec![Published::Opening; n]),
        }
    }

    /// The root holding `path`, by its spelling, and the path below it.
    pub fn locate(&self, path: &Path) -> Option<(usize, PathBuf)> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, r)| path.starts_with(r))
            .max_by_key(|(_, r)| r.components().count())
            .map(|(i, r)| {
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
    Stop,
}

/// A folder a client keeps out of the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Registration {
    client: String,
    path: WirePath,
}

/// The files of what clients registered, in the state folder: folders
/// kept out of the index, and folders kept live.
const EXCLUDED: &str = "registered";
const WATCHED: &str = "watched";

/// What clients registered, kept across restarts.
fn registered(dirs: &Dirs, file: &str) -> Vec<Registration> {
    std::fs::read(dirs.state.join(file))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_registered(dirs: &Dirs, file: &str, all: &[Registration]) {
    let path = dirs.state.join(file);
    let tmp = dirs.state.join(format!("{file}.tmp"));
    let ok = crate::dirs::make_private(&dirs.state).is_ok()
        && serde_json::to_vec(all).is_ok_and(|b| std::fs::write(&tmp, b).is_ok())
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

/// Every exclusion: the defaults, the user's, what clients registered;
/// and how many were refused.
fn exclusions(dirs: &Dirs, registrations: &[Registration]) -> (Exclusions, usize) {
    let paths = skwird_crawl::exclude::defaults(&dirs.home)
        .into_iter()
        .chain(configured(dirs))
        .chain(registrations.iter().map(|r| r.path.to_path()));
    let (exclusions, refused) = Exclusions::resolve(paths);
    (exclusions, refused.len())
}

fn log_refused(refused: usize) {
    if refused > 0 {
        log(&format!(
            "{refused} exclusions refused: empty, relative or unresolvable"
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
pub fn writer(
    config: &Config,
    shared: &Shared,
    commands: &Receiver<Command>,
    watcher: Option<Arc<Watcher>>,
) {
    let opts = Options {
        watcher: watcher.clone(),
        threads: config.threads,
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
    let cookies = watcher
        .as_ref()
        .and_then(|w| cookie_folder(&config.dirs, w));
    let (keepers, built) = open_all(config, shared, &registrations, &opts, &cancel);
    if built {
        // What a first build freed, given back now that it is saved.
        crate::memory::give_back();
    }
    let mut w = Writing {
        config,
        shared,
        cancel,
        keepers,
        registrations,
        kept_live,
        watcher,
        gather: Gather::default(),
        cookies,
        next_cookie: 0,
        waiting: Vec::new(),
        pending: Vec::new(),
    };
    w.apply_scopes();
    loop {
        let command = match w.pending.pop() {
            Some(c) => c,
            None => match w.deadline() {
                None => match commands.recv() {
                    Ok(c) => c,
                    Err(_) => break,
                },
                Some(at) => {
                    match commands.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(c) => c,
                        Err(RecvTimeoutError::Timeout) => {
                            w.on_time();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            },
        };
        if !w.answer(commands, command) {
            break;
        }
        // Heard without pause, the deadline would never pass.
        w.on_time();
    }
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
/// is ready. Says whether any was built.
fn open_all(
    config: &Config,
    shared: &Shared,
    registrations: &[Registration],
    opts: &Options,
    cancel: &Cancel,
) -> (Vec<Option<Keeper>>, bool) {
    let mut keepers = Vec::new();
    let mut built = false;
    let (ex, refused) = exclusions(&config.dirs, registrations);
    log_refused(refused);
    for (i, path) in config.roots.iter().enumerate() {
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
                match how {
                    Opened::Loaded { reconciled, .. } => log_reconciled(i, reconciled),
                    Opened::Built { .. } => built = true,
                }
                shared.publish(i, Published::Ready(Arc::new(keeper.snapshot())));
                keepers.push(Some(keeper));
            }
            Err(e) => {
                log(&format!("root {i} cannot be opened: {e}"));
                shared.publish(i, Published::Failed(e.to_string()));
                keepers.push(None);
            }
        }
    }
    (keepers, built)
}

/// What the writer holds.
struct Writing<'a> {
    config: &'a Config,
    shared: &'a Shared,
    cancel: Cancel,
    keepers: Vec<Option<Keeper>>,
    /// Folders clients keep out of the index.
    registrations: Vec<Registration>,
    /// Folders clients keep live.
    kept_live: Vec<Registration>,
    watcher: Option<Arc<Watcher>>,
    /// What was heard and not applied yet, and what is due.
    gather: Gather,
    /// The folder cookies are made in, once watched.
    cookies: Option<PathBuf>,
    next_cookie: u64,
    /// Syncs waiting for their cookie.
    waiting: Vec<Waiting>,
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
                // Live, a barrier serves: a cookie heard in its turn says
                // every change before it was heard (DESIGN 2). Otherwise,
                // the check.
                if live && let Some(cookie) = self.make_cookie() {
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
                let answer = match (self.keepers[root].as_ref(), Token::parse(&since)) {
                    (Some(keeper), Some(token)) => keeper.changes(&rel, token),
                    _ => Err(TooOld),
                };
                let _ = reply.send(answer);
            }
            Command::Exclude {
                client,
                add,
                remove,
                reply,
            } => {
                self.register(&client, &add, &remove);
                let _ = reply.send(theirs(&self.registrations, &client));
            }
            Command::Watch {
                client,
                add,
                remove,
                reply,
            } => {
                if changed(&mut self.kept_live, &client, &add, &remove) {
                    save_registered(&self.config.dirs, WATCHED, &self.kept_live);
                    self.apply_scopes();
                }
                let _ = reply.send(theirs(&self.kept_live, &client));
            }
            Command::Heard { generation, heard } => self.heard(generation, heard),
            Command::Mounts => self.mounts_changed(),
        }
        true
    }

    /// What the watcher heard: each folder named gathered, an overflow or
    /// a watched subtree's own folder lost lowering coverage at once, the
    /// check due once the queue is quiet.
    fn heard(&mut self, generation: u64, heard: Vec<Heard>) {
        let now = Instant::now();
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
        for (root, folders) in self.gather.due(now) {
            self.apply_heard(root, &folders);
        }
        if self.gather.recheck_due(now) {
            for (i, keeper) in self.keepers.iter_mut().enumerate() {
                if let Some(keeper) = keeper {
                    refresh(keeper, i, self.shared, &self.cancel);
                }
            }
        }
        if self.gather.save_due(now) {
            for keeper in self.keepers.iter_mut().flatten() {
                if keeper.save().is_err() {
                    log("a save failed: the index is kept in memory, the log on disk");
                }
            }
            crate::memory::give_back();
        }
    }

    /// Every gathering applied at once, as the engine stops.
    fn flush_all(&mut self) {
        for root in 0..self.keepers.len() {
            let heard = self.gather.take(root);
            self.apply_heard(root, &heard);
        }
    }

    /// The folders of `root` heard, read again.
    fn apply_heard(&mut self, root: usize, folders: &[FolderId]) {
        if folders.is_empty() {
            return;
        }
        let Some(keeper) = self.keepers[root].as_mut() else {
            return;
        };
        match keeper.heard(folders, &self.cancel) {
            Ok(done) if !done.records.is_empty() => self.gather.changed(Instant::now()),
            Ok(_) => {}
            Err(e) => log(&format!(
                "root {root}: what was heard cannot be applied: {e}"
            )),
        }
        self.publish(root);
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
        let (ex, refused) = exclusions(&self.config.dirs, &self.registrations);
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
    fn register(&mut self, client: &str, add: &[PathBuf], remove: &[PathBuf]) {
        if !changed(&mut self.registrations, client, add, remove) {
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
fn changed(all: &mut Vec<Registration>, client: &str, add: &[PathBuf], remove: &[PathBuf]) -> bool {
    let mut changed = false;
    for path in add.iter().filter(|p| p.is_absolute()) {
        let r = Registration {
            client: client.to_owned(),
            path: WirePath::of(path),
        };
        if !all.contains(&r) {
            all.push(r);
            changed = true;
        }
    }
    let before = all.len();
    all.retain(|r| !(r.client == client && remove.iter().any(|p| WirePath::of(p) == r.path)));
    changed | (all.len() != before)
}

/// What `client` registered.
fn theirs(all: &[Registration], client: &str) -> Vec<PathBuf> {
    all.iter()
        .filter(|r| r.client == client)
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

    /// A sync served by the check: what was heard applied first, the
    /// user's exclusions read again, every folder of the root compared.
    fn check_now(&mut self, root: usize, replies: Vec<Sender<()>>) {
        let heard = self.gather.take(root);
        self.apply_heard(root, &heard);
        // A line added to the user's file holds from the next sync.
        self.hold_to_exclusions();
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
