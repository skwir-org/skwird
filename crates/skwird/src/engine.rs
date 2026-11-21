//! The engine's writer: one thread owns each root's index and its log
//! (`skwird_index::keeper`), and is the only one to change them. Queries
//! read the snapshot it publishes, swapped whole: one writer, so no write
//! conflicts to resolve; queries hold no lock while they run.
//!
//! It opens each root (its index read back, or crawled), then waits for a
//! command: a sync (the check, run now; syncs asked meanwhile served by
//! the same refresh), the journal, a client's registrations. Nothing wakes
//! it otherwise: idle is idle.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use skwird_crawl::{Cancel, Exclusions, Options, Root};
use skwird_index::Index;
use skwird_index::keeper::{Event, Keeper, Opened, Reconciled, TooOld};
use skwird_index::log::Token;
use skwird_index::tree::Limits;
use skwird_protocol::WirePath;

use crate::dirs::Dirs;

/// How an engine runs.
#[derive(Clone, Debug)]
pub struct Config {
    pub dirs: Dirs,
    /// The roots it covers, as the system resolves them.
    pub roots: Vec<PathBuf>,
    /// How long it stays once its last client left: none for always.
    pub idle: Option<std::time::Duration>,
    pub threads: usize,
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
    Stop,
}

/// A folder a client keeps out of the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Registration {
    client: String,
    path: WirePath,
}

/// What clients registered, kept across restarts.
fn registered(dirs: &Dirs) -> Vec<Registration> {
    std::fs::read(dirs.state.join("registered"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_registered(dirs: &Dirs, all: &[Registration]) {
    let path = dirs.state.join("registered");
    let tmp = dirs.state.join("registered.tmp");
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
/// stop or every sender is gone.
pub fn writer(config: &Config, shared: &Shared, commands: &Receiver<Command>) {
    let opts = Options {
        threads: config.threads,
        ..Options::default()
    };
    let registrations = registered(&config.dirs);
    let cancel = Cancel::default();
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
        pending: Vec::new(),
    };
    loop {
        let command = match w.pending.pop() {
            Some(c) => c,
            None => match commands.recv() {
                Ok(c) => c,
                Err(_) => break,
            },
        };
        if !w.answer(commands, command) {
            break;
        }
    }
    for keeper in w.keepers.iter_mut().flatten() {
        if keeper.save().is_err() {
            log("a save at exit failed: the log keeps what changed");
        }
    }
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
        let start = std::time::Instant::now();
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
    registrations: Vec<Registration>,
    /// Commands taken while gathering syncs, to answer next.
    pending: Vec<Command>,
}

impl Writing<'_> {
    /// Answer one command: false to stop.
    fn answer(&mut self, commands: &Receiver<Command>, command: Command) -> bool {
        match command {
            Command::Stop => return false,
            Command::Sync { root, reply } => {
                // Every sync asked of this root meanwhile is served by the
                // same refresh; others wait their turn.
                let mut replies = vec![reply];
                while let Ok(next) = commands.try_recv() {
                    match next {
                        Command::Sync { root: r, reply } if r == root => replies.push(reply),
                        other => self.pending.insert(0, other),
                    }
                }
                // The user's exclusions read again first: a line added to
                // their file holds from the next sync.
                self.hold_to_exclusions();
                if let Some(keeper) = self.keepers[root].as_mut() {
                    refresh(keeper, root, self.shared, &self.cancel);
                }
                for reply in replies {
                    let _ = reply.send(());
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
                let theirs = self
                    .registrations
                    .iter()
                    .filter(|r| r.client == client)
                    .map(|r| r.path.to_path())
                    .collect();
                let _ = reply.send(theirs);
            }
        }
        true
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
        let mut changed = false;
        for path in add.iter().filter(|p| p.is_absolute()) {
            let r = Registration {
                client: client.to_owned(),
                path: WirePath::of(path),
            };
            if !self.registrations.contains(&r) {
                self.registrations.push(r);
                changed = true;
            }
        }
        let before = self.registrations.len();
        self.registrations
            .retain(|r| !(r.client == client && remove.iter().any(|p| WirePath::of(p) == r.path)));
        changed |= self.registrations.len() != before;
        if !changed {
            return;
        }
        // Kept first: a kill before the purge's save leaves it to the next
        // open, which holds the index to what is kept.
        save_registered(&self.config.dirs, &self.registrations);
        let (readmitting, refused) = self.hold_to_exclusions();
        log_refused(refused);
        for i in readmitting {
            if let Some(keeper) = self.keepers[i].as_mut() {
                refresh(keeper, i, self.shared, &self.cancel);
            }
        }
    }
}

fn refresh(keeper: &mut Keeper, root: usize, shared: &Shared, cancel: &Cancel) {
    let start = std::time::Instant::now();
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
}
