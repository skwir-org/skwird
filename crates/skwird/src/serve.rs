//! The engine's socket: one engine per user, held by a lock; every peer
//! checked to be its user; a connection per thread; a query on a thread
//! of its own, so that a `cancel` can reach it; the engine gone once its
//! last client has been gone a while.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skwird_index::block::FolderId;
use skwird_index::keeper::Event;
use skwird_index::{Index, NameQuery, Pattern, Query, Sight};
use skwird_protocol::{self as wire, Answer, Ask, Request, Response, WirePath};

use crate::asker::{Asker, Seeing};
use crate::convert;
use crate::coverage::coverage_at;
use crate::dirs::{check_socket_path, make_private};
use crate::engine::{Command, Config, Published, Shared, log, writer};
use crate::transport::{Listener, Stream};

/// Results sent at a time.
pub(crate) const BATCH: usize = 512;

/// At the system service, which answers any local user (DESIGN 4): the
/// most results a query gives, and the most queries a user runs at once.
const MOST_RESULTS: u64 = 100_000;
const MOST_QUERIES: usize = 4;
/// And the most connections a user holds open at once.
const MOST_CONNECTIONS: usize = 64;

/// Set once the engine is asked to stop by its service manager (Windows',
/// [`stop`]): its loop ends at its next wake, as when it is idle.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// The writer's channel, for what the service manager tells of volumes
/// (Windows): set as the engine serves.
#[cfg(windows)]
static COMMANDS: Mutex<Option<Sender<Command>>> = Mutex::new(None);

/// A volume arrived, or went (Windows' service manager): the writer
/// follows its roots again, as when Linux's mount table changes; and
/// again a few seconds after, a volume arrived being given its letter
/// after it is told of.
#[cfg(windows)]
pub fn volumes_changed() {
    let sender = COMMANDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(sender) = sender else { return };
    log("a volume arrived or went: the roots followed again");
    let _ = sender.send(Command::Mounts);
    let _ = std::thread::Builder::new()
        .name("skwird-volumes".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let _ = sender.send(Command::Mounts);
        });
}

/// The removal of root `root`'s volume is asked (Windows' service
/// manager): the writer lets go of the root's handles, and this gives
/// back once it did, so that the removal goes on, or after 10 s.
#[cfg(windows)]
pub fn volume_leaving(root: usize) {
    let sender = COMMANDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(sender) = sender else { return };
    let (reply, done) = channel();
    if sender.send(Command::LetGo { root, reply }).is_ok() {
        let _ = done.recv_timeout(Duration::from_secs(10));
    }
}

/// Stop serving on `pipe` (Windows' service manager's stop): the loop
/// that waits for clients is woken by a connection to its own pipe, and
/// ends as when idle, the writer applying what it heard and saving every
/// root, its journal's position among it, before [`serve`] gives back.
#[cfg(windows)]
pub fn stop(pipe: &Path) {
    STOPPING.store(true, Ordering::Release);
    let _ = skwird_client::pipe::Stream::connect(pipe.as_os_str(), Some(Duration::from_secs(1)));
}

/// How many clients are connected, and since when none is.
struct Live {
    state: Mutex<(usize, Instant)>,
}

impl Live {
    fn join(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0 += 1;
    }

    fn leave(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 -= 1;
        if state.0 == 0 {
            state.1 = Instant::now();
        }
    }

    /// How long until the engine may go: none while a client is here.
    fn left(&self, idle: Duration) -> Option<Duration> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.0 == 0).then(|| idle.saturating_sub(state.1.elapsed()))
    }
}

/// Serve the user's engine until it has been idle for `config.idle`, or
/// for ever, or until its service manager stops it (Windows, [`stop`]). A
/// second engine blocks on the lock until the first is gone, then takes
/// over: it never polls the lock.
pub fn serve(config: &Config) -> Result<(), String> {
    // Before any thread allocates: what a burst frees goes back.
    crate::memory::settle();
    let socket = config.dirs.socket();
    check_socket_path(&socket)?;
    // The system service's socket is every user's to reach; a user's, its
    // user's alone.
    if config.system {
        crate::dirs::make_system(&config.dirs)?;
    } else {
        make_private(&config.dirs.runtime)?;
    }
    // On Windows, the system service reads every folder whatever its
    // descriptor says (the backup privilege), and numbers its users by
    // their SIDs, as its state keeps them.
    #[cfg(windows)]
    if config.system {
        if !skwird_crawl::read_anything() {
            log(
                "the backup privilege is not held: a folder whose descriptor denies the service \
                 is a doubt, not covered",
            );
        }
        crate::system::keep_numbers(&config.dirs.state);
    }
    let lock = crate::os::lock(&config.dirs.lock())?;
    let (listener, bound) = listen(config, &socket)?;
    log(&format!("serving {} roots", config.roots.len()));
    // What this engine may not read without asking (DESIGN 4), decided once.
    #[cfg(target_os = "macos")]
    {
        // The system service's, below every user's home.
        let withheld = if config.system {
            let homes: Vec<std::path::PathBuf> = crate::system::users()
                .into_iter()
                .map(|(_, home)| home)
                .collect();
            skwird_crawl::withhold_consent_below(&homes, std::path::Path::new(crate::system::HOMES))
                .len()
        } else {
            skwird_crawl::consent_withheld().len()
        };
        if withheld > 0 {
            log(&format!(
                "{withheld} folders macOS asks consent for are not read: no Full Disk Access"
            ));
        }
    }
    let shared = Arc::new(Shared::new(config.roots.clone(), config.system));
    let (commands, received) = channel();
    #[cfg(windows)]
    {
        *COMMANDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(commands.clone());
    }
    let (watcher, group) = sources(config, &commands, &shared);
    let writing = {
        let (config, shared, sender) = (config.clone(), Arc::clone(&shared), commands.clone());
        named("skwird-writer", move || {
            writer(&config, &shared, &received, sender, watcher, group);
        })
    };
    let live = Arc::new(Live {
        state: Mutex::new((0, Instant::now())),
    });
    loop {
        // Woken by a client, or when the idle time may be up: no polling.
        let wait = config.idle.map(|idle| live.left(idle).unwrap_or(idle));
        if wait.is_some_and(|w| w.is_zero()) {
            break;
        }
        let accepted = listener.accept(wait);
        if STOPPING.load(Ordering::Acquire) {
            break;
        }
        let stream = match accepted {
            Ok(Some(stream)) => stream,
            Ok(None) => continue,
            Err(e) => return Err(format!("cannot wait for clients: {e}")),
        };
        // A user's engine answers its user alone; the system service, every
        // user, each what they could list (rule 2).
        let Ok(asker) = Asker::of(&stream) else {
            continue;
        };
        if !config.system && !asker.is_engines_user() {
            continue;
        }
        // Each connection is a thread of the service's: a user holds at most
        // so many at once.
        let Some(open) = Running::connection(&shared, asker.uid) else {
            continue;
        };
        live.join();
        let (shared, commands, live) = (Arc::clone(&shared), commands.clone(), Arc::clone(&live));
        named("skwird-client", move || {
            connection(stream, &shared, &commands, &asker);
            drop(open);
            live.leave();
        });
    }
    let _ = commands.send(Command::Stop);
    let _ = writing.join();
    // A socket the service manager passed stays its own, to start the
    // next engine. A pipe is no file.
    if bound && cfg!(unix) {
        let _ = std::fs::remove_file(&socket);
    }
    log("idle: gone");
    drop(lock);
    Ok(())
}

/// Where the engine answers: a service manager's socket, where it passed
/// one; otherwise its own, bound (and so to be removed). The lock is held:
/// a socket left at the path is a dead engine's.
#[cfg(unix)]
fn listen(config: &Config, socket: &Path) -> Result<(Listener, bool), String> {
    #[cfg(target_os = "linux")]
    if let Some(listener) = crate::activation::passed()? {
        log("answering on the socket the service manager passed");
        return Ok((Listener::passed(listener), false));
    }
    let _ = std::fs::remove_file(socket);
    let listener = Listener::bind(socket).map_err(|e| format!("cannot bind the socket: {e}"))?;
    if config.system {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| format!("cannot open the socket to every user: {e}"))?;
    }
    Ok((listener, true))
}

/// Where the engine answers on Windows: its user's pipe, made its user's
/// alone, refused if another process holds the name.
///
/// The lock is held, so no other engine of the user's serves; but one
/// that just went may still hold the name for a moment. Windows closes a
/// dying process's handles one after the other, its lock's before its
/// pipe's at times: a second engine taking over found the name held, once
/// in four runs on a Windows runner. The name is asked for again for up to
/// five seconds, while it is refused (`ERROR_ACCESS_DENIED`); a name held
/// longer is another process's, and is refused.
///
/// The system service's pipe is every user's to reach, and its owner
/// `LocalSystem`, which no other user can make it (DESIGN, Protocol).
#[cfg(windows)]
fn listen(config: &Config, pipe: &Path) -> Result<(Listener, bool), String> {
    const DENIED: i32 = 5;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let bound = if config.system {
            Listener::bind_system(pipe)
        } else {
            Listener::bind(pipe)
        };
        match bound {
            Ok(l) => return Ok((l, true)),
            Err(e) if e.raw_os_error() == Some(DENIED) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(format!(
                    "cannot make the pipe (another process may hold it): {e}"
                ));
            }
        }
    }
}

/// What the engine hears changes by: one watcher for every root, or for
/// the system service one group marking whole filesystems, each listened
/// to on a thread of its own that waits on it alone.
fn sources(
    config: &Config,
    commands: &Sender<Command>,
    shared: &Arc<Shared>,
) -> (
    Option<Arc<skwird_crawl::Watcher>>,
    Option<Arc<skwird_crawl::fanotify::Group>>,
) {
    // On Windows, the system service's roots are each heard whole, through
    // their volumes' journals, by the watcher a user's engine has.
    let (watcher, group) = if config.system && cfg!(target_os = "linux") {
        (None, crate::live::group())
    } else {
        (crate::live::watcher(config.watches), None)
    };
    // inotify and fanotify, each waited on with the mount table: Linux's.
    #[cfg(target_os = "linux")]
    {
        if let Some(w) = &watcher {
            let (w, commands, shared) = (Arc::clone(w), commands.clone(), Arc::clone(shared));
            named("skwird-heard", move || {
                crate::live::listen(&*w, &commands, &shared.readers);
            });
        }
        if let Some(g) = &group {
            let (g, commands, shared) = (Arc::clone(g), commands.clone(), Arc::clone(shared));
            named("skwird-heard", move || {
                crate::live::listen(&*g, &commands, &shared.readers);
            });
        }
    }
    // Windows' watcher hears each root on a thread of its own, macOS' in
    // one stream, and each is read here for the writer.
    #[cfg(any(windows, target_os = "macos"))]
    if let Some(w) = &watcher {
        let (w, commands, shared) = (Arc::clone(w), commands.clone(), Arc::clone(shared));
        named("skwird-heard", move || {
            crate::live::listen(&w, &commands, &shared.readers);
        });
    }
    // macOS' mounts, followed for the removable volumes a user's engine
    // keeps (`crate::volumes`).
    #[cfg(target_os = "macos")]
    if !config.system {
        let commands = commands.clone();
        named("skwird-mounts", move || {
            crate::live::follow_mounts(&commands);
        });
    }
    (watcher, group)
}

pub(crate) type Writer = Arc<Mutex<Stream>>;

pub(crate) fn send(to: &Writer, id: u64, answer: Answer) -> bool {
    let mut stream = to.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    wire::write(&mut *stream, &Response { id, answer }).is_ok()
}

pub(crate) fn failure(what: &str, message: impl Into<String>) -> Answer {
    Answer::Error(wire::Failure {
        what: what.to_owned(),
        message: message.into(),
    })
}

pub(crate) fn outside() -> wire::Coverage {
    wire::Coverage {
        names: wire::Proof::NotCovered(wire::Why::Outside),
        attrs: wire::Proof::NotCovered(wire::Why::Outside),
        holes: Vec::new(),
    }
}

/// The coverage below `root`, from the snapshot published now.
/// How far answers below `root` can be trusted, for `asker`: the roots
/// below it (the system service's other filesystems) each at its place.
fn coverage(shared: &Shared, root: &WirePath, asker: &Asker) -> Result<wire::Coverage, Answer> {
    let path = root.to_path();
    let mut c = coverage_one(shared, root, asker)?;
    if matches!(c.names, wire::Proof::NotCovered(_)) {
        return Ok(c);
    }
    let Some((i, _)) = shared.locate(&path) else {
        return Ok(c);
    };
    let mut below = shared.roots_below(&path, i);
    below.sort_by_key(|&j| shared.root(j).components().count());
    for j in below {
        if let Ok(inner) = coverage_one(shared, &WirePath::of(&shared.root(j)), asker) {
            c = at_root_below(c, &shared.root(j), inner);
        }
    }
    Ok(c)
}

/// [`coverage`], of the root holding `root` alone.
fn coverage_one(shared: &Shared, root: &WirePath, asker: &Asker) -> Result<wire::Coverage, Answer> {
    let path = root.to_path();
    let Some((i, rel)) = shared.locate(&path) else {
        return Ok(outside());
    };
    match shared.snapshot(i) {
        Published::Opening => Ok(wire::Coverage {
            names: wire::Proof::NotCovered(wire::Why::Unread),
            attrs: wire::Proof::NotCovered(wire::Why::Unread),
            holes: Vec::new(),
        }),
        Published::Failed(why) => Err(failure("internal", why)),
        Published::Ready(index) => {
            let seeing = seeing(shared, i, &index, asker);
            let mut memo = HashMap::new();
            let mut holes = Vec::new();
            if let Some(seeing) = &seeing {
                match reached(seeing, &rel, &mut memo) {
                    Reached::Denied => return Ok(denied()),
                    Reached::Absent => {}
                    Reached::Listed(folder) => {
                        holes = seeing.acl_holes(folder, &mut memo);
                    }
                }
            }
            let mut c = coverage_at(&index, &shared.root(i), &rel)
                .ok_or_else(|| failure("not_a_folder", "no folder of the index is at that path"))?;
            // A hole is a folder's name: said only to who may list its
            // parent, never another user's private folder's.
            if let Some(seeing) = &seeing {
                seeing.seen_holes(&shared.root(i), &mut c.holes, &mut memo);
            }
            c.holes
                .extend(holes.into_iter().map(|f| hole(shared, i, &index, f)));
            Ok(c)
        }
    }
}

/// What the asker sees of root `i`'s index: none where they see all of it
/// (a user's engine, or root asking the system service).
pub(crate) fn seeing<'a>(
    shared: &Shared,
    i: usize,
    index: &'a Index,
    asker: &'a Asker,
) -> Option<Seeing<'a>> {
    (shared.system && !asker.is_root()).then(|| Seeing {
        index,
        asker,
        above: above(shared, i, asker),
    })
}

/// Whether `asker` may search every folder above root `i`: each as the
/// index of the root holding it says now, live, so that a folder closed
/// since the start (`chmod 700 ~` above a filesystem of its own) closes
/// what is below it; as it was at the start where no root holds it.
fn above(shared: &Shared, i: usize, asker: &Asker) -> Sight {
    let root = shared.root(i);
    let mut ancestors: Vec<&Path> = root.ancestors().skip(1).collect();
    ancestors.reverse();
    let at_start = shared.above(i);
    ancestors
        .iter()
        .enumerate()
        .fold(Sight::Seen, |sight, (k, folder)| {
            let held = shared
                .locate(folder)
                .filter(|(j, _)| *j != i)
                .and_then(|(j, rel)| match shared.snapshot(j) {
                    Published::Ready(index) => {
                        let facts = index.facts_of(index.folder_at(&rel)?)?;
                        let owned = crate::asker::Owned {
                            owner: facts.owner,
                            group: facts.group,
                            mode: facts.mode,
                            dev: facts.id.dev,
                        };
                        Some(asker.may_by(owned, index.acl_of(facts.id), 0o1))
                    }
                    _ => None,
                });
            let searched = held.unwrap_or_else(|| {
                at_start
                    .get(k)
                    .copied()
                    .flatten()
                    .map_or(Sight::Hidden, |f| asker.may(f, 0o1))
            });
            sight.and(searched)
        })
}

/// How far an asker reaches the folder at a path.
pub(crate) enum Reached {
    /// They may list it.
    Listed(FolderId),
    /// The index holds no folder there, and they may list the nearest one
    /// above: they may know it is not there.
    Absent,
    /// They may not list it, or the nearest folder above it the index
    /// holds: whether it exists is not said either.
    Denied,
}

pub(crate) fn reached(
    seeing: &Seeing<'_>,
    rel: &Path,
    memo: &mut HashMap<FolderId, Sight>,
) -> Reached {
    let Some((at, folder)) = rel
        .ancestors()
        .find_map(|a| seeing.index.folder_at(a).map(|f| (a, f)))
    else {
        return Reached::Denied;
    };
    // A folder never stat-ed (excluded by its path, or not read) holds no
    // facts to judge it by. Its entry is in its parent's listing: an asker
    // who may list the parent is told what the index says of it, excluded
    // among it, so that a client walks it itself (DESIGN 5).
    let sight = match seeing.index.parent_of(folder) {
        Some(parent) if at == rel && seeing.index.facts_of(folder).is_none() => {
            seeing.sight(parent, memo)
        }
        _ => seeing.sight(folder, memo),
    };
    match (sight, at == rel) {
        (Sight::Seen, true) => Reached::Listed(folder),
        (Sight::Seen, false) => Reached::Absent,
        _ => Reached::Denied,
    }
}

/// Not covered, for an asker who may not list the folder asked.
pub(crate) fn denied() -> wire::Coverage {
    wire::Coverage {
        names: wire::Proof::NotCovered(wire::Why::Denied),
        attrs: wire::Proof::NotCovered(wire::Why::Denied),
        holes: Vec::new(),
    }
}

/// Folder `f` of root `i`'s index, not covered for an asker.
fn hole(shared: &Shared, i: usize, index: &Index, f: FolderId) -> wire::Hole {
    wire::Hole {
        path: WirePath::of(&shared.root(i).join(index.path_of(f))),
        why: wire::Why::Denied,
    }
}

/// The client's hello, answered: its name, or none if it speaks no
/// version of ours, or said something else first.
fn hello(reader: &mut BufReader<Stream>, writer: &Writer, shared: &Shared) -> Option<String> {
    match wire::read::<_, Request>(reader) {
        Ok(Some(Request {
            id,
            ask: Ask::Hello(hello),
        })) => {
            if !hello.versions.contains(&wire::VERSION) {
                send(
                    writer,
                    id,
                    failure("version", "this engine speaks protocol 0 alone"),
                );
                return None;
            }
            let welcome = wire::Welcome {
                version: wire::VERSION,
                scope: if shared.system {
                    wire::Scope::System
                } else {
                    wire::Scope::User
                },
                engine: concat!("skwird ", env!("CARGO_PKG_VERSION")).to_owned(),
                roots: shared.roots().iter().map(|r| WirePath::of(r)).collect(),
                features: features(shared.system),
            };
            send(writer, id, Answer::Hello(welcome)).then_some(hello.client)
        }
        Ok(Some(Request { id, .. })) => {
            send(writer, id, failure("bad_request", "hello comes first"));
            None
        }
        _ => None,
    }
}

/// What a user holds of the system service, counted while it is held:
/// queries running, or connections open. Given back when dropped.
struct Held {
    counts: Arc<Mutex<HashMap<u32, usize>>>,
    uid: u32,
}

impl Held {
    /// One more for user `uid`, or none if they hold `most` already (none
    /// is the most: a user's engine counts, but never refuses).
    fn take(
        counts: &Arc<Mutex<HashMap<u32, usize>>>,
        uid: u32,
        most: Option<usize>,
    ) -> Option<Held> {
        let mut held = counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = held.entry(uid).or_insert(0);
        if most.is_some_and(|most| *n >= most) {
            return None;
        }
        *n += 1;
        Some(Held {
            counts: Arc::clone(counts),
            uid,
        })
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut held = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = held.get_mut(&self.uid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.remove(&self.uid);
            }
        }
    }
}

/// A query a user runs: at the system service, at most [`MOST_QUERIES`]
/// at once.
type Running = Held;

impl Running {
    fn query(shared: &Shared, uid: u32) -> Option<Running> {
        Held::take(&shared.running, uid, shared.system.then_some(MOST_QUERIES))
    }

    /// A connection a user holds open: at the system service, at most
    /// [`MOST_CONNECTIONS`] at once, each a thread of its own.
    fn connection(shared: &Shared, uid: u32) -> Option<Held> {
        Held::take(
            &shared.connections,
            uid,
            shared.system.then_some(MOST_CONNECTIONS),
        )
    }
}

/// How often a user other than root asks the system service's writer for
/// work: a sync, a registration, the journal.
const ASK_EVERY: Duration = Duration::from_secs(1);

/// Whether `who` asks the writer too soon after their last ask, which is
/// then refused; otherwise this ask is noted.
fn too_soon(shared: &Shared, who: &Asker) -> bool {
    if !shared.system || who.is_root() {
        return false;
    }
    let mut asked = shared
        .asked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if asked
        .get(&who.uid)
        .is_some_and(|&last| now.duration_since(last) < ASK_EVERY)
    {
        return true;
    }
    asked.insert(who.uid, now);
    false
}

/// What a request answered on a thread of its own does: a query, or a
/// `totals`, which asks the writer for its journal's token.
fn apart(
    commands: &Sender<Command>,
    id: u64,
    ask: Ask,
) -> impl FnOnce(&Shared, &Writer, &AtomicBool, &Asker) -> Answer + Send + 'static {
    let commands = commands.clone();
    move |shared: &Shared, writer: &Writer, flag: &AtomicBool, who: &Asker| match ask {
        Ask::Query(query) => answer_query(shared, writer, id, &query, flag, who),
        Ask::Totals { root, largest } => {
            crate::totals::answer(shared, &commands, writer, id, &root, largest, flag, who)
        }
        _ => failure("internal", "not a query"),
    }
}

/// A query answered on a thread of its own, so that a `cancel` can reach
/// it, its flag kept in `cancels` while it runs.
fn spawn_query(
    shared: &Arc<Shared>,
    writer: &Writer,
    cancels: &Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    id: u64,
    who: &Asker,
    running: Running,
    work: impl FnOnce(&Shared, &Writer, &AtomicBool, &Asker) -> Answer + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let flag = Arc::new(AtomicBool::new(false));
    cancels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, Arc::clone(&flag));
    let (shared, writer, cancels, who) = (
        Arc::clone(shared),
        Arc::clone(writer),
        Arc::clone(cancels),
        who.clone(),
    );
    named("skwird-query", move || {
        let answer = work(&shared, &writer, &flag, &who);
        cancels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        send(&writer, id, answer);
        drop(running);
    })
}

/// A client's `volumes` (`keep`, `forget`, `release`), asked of the
/// writer, which keeps them: each volume indexed answered its coverage as
/// `coverage` answers it. The system service keeps none.
fn volumes(
    shared: &Shared,
    commands: &Sender<Command>,
    client: &str,
    ask: Ask,
    who: &Asker,
) -> Answer {
    let Ask::Volumes {
        keep,
        forget,
        release,
        release_at,
    } = ask
    else {
        return failure("internal", "not a volumes");
    };
    if shared.system {
        return failure("not_here", "the system service keeps no removable volume");
    }
    let (reply, answer) = channel();
    let asked = commands.send(Command::Volumes {
        client: client.to_owned(),
        keep,
        forget,
        release,
        release_at: release_at.iter().map(WirePath::to_path).collect(),
        reply,
    });
    match asked.ok().and_then(|()| answer.recv().ok()) {
        Some(Ok(crate::engine::Released {
            mut volumes,
            reading,
        })) => {
            // Nothing of a volume let go of held when the client is
            // answered: it unmounts the volume as it is.
            let until = Instant::now() + crate::volumes::RELEASE_WAIT;
            for r in &reading {
                if !r.ended_within(until.saturating_duration_since(Instant::now())) {
                    log("a volume released is still read: answered all the same");
                }
            }
            for v in &mut volumes.kept {
                if v.standing == wire::Standing::Indexed
                    && let Some(at) = &v.at
                {
                    v.coverage = coverage(shared, at, who).ok();
                }
            }
            Answer::Volumes(volumes)
        }
        Some(Err(why)) => failure("not_here", why),
        None => failure("internal", "the engine is going"),
    }
}

/// One client: hello first, then its requests in turn, each query on a
/// thread of its own.
fn connection(stream: Stream, shared: &Arc<Shared>, commands: &Sender<Command>, who: &Asker) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let writer: Writer = Arc::new(Mutex::new(clone));
    let mut reader = BufReader::new(stream);
    let Some(client) = hello(&mut reader, &writer, shared) else {
        return;
    };
    let cancels: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::default();
    let mut queries = Vec::new();
    loop {
        let request = match wire::read::<_, Request>(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                send(&writer, 0, failure("bad_request", e.to_string()));
                break;
            }
        };
        let id = request.id;
        let answer = match request.ask {
            Ask::Hello(_) => failure("bad_request", "hello comes once"),
            Ask::Coverage { root } => match coverage(shared, &root, who) {
                Ok(c) => Answer::Coverage(c),
                Err(a) => a,
            },
            // The writer is the whole service's: at the system service, a
            // user other than root asks it for work at most once a second.
            Ask::Sync { .. } | Ask::Exclude { .. } | Ask::Changes { .. }
                if too_soon(shared, who) =>
            {
                failure("busy", "the system service takes this once a second a user")
            }
            Ask::Sync { root } => sync(shared, commands, &root, who),
            ask @ (Ask::Query(_) | Ask::Totals { .. }) => {
                let Some(running) = Running::query(shared, who.uid) else {
                    send(
                        &writer,
                        id,
                        failure("busy", "this user's queries at once are at the most"),
                    );
                    continue;
                };
                // A thread that ended is let go of now: kept to the end of a
                // long connection, each would hold its stack until then.
                queries.retain(|q: &std::thread::JoinHandle<()>| !q.is_finished());
                let work = apart(commands, id, ask);
                queries.push(spawn_query(
                    shared, &writer, &cancels, id, who, running, work,
                ));
                continue;
            }
            Ask::Cancel { id: of } => {
                if let Some(flag) = cancels
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&of)
                {
                    flag.store(true, Ordering::Relaxed);
                }
                // The query answers itself: its `done` says it was
                // cancelled.
                continue;
            }
            Ask::Watch { add, .. } if shared.system => watching_all(shared, add, who),
            Ask::Changes { root, since } => changes(shared, commands, &root, since, who),
            Ask::Exclude { add, remove } => {
                let (reply, answer) = channel();
                let to_path = |v: Vec<WirePath>| v.iter().map(WirePath::to_path).collect();
                let asked = commands.send(Command::Exclude {
                    client: client.clone(),
                    uid: shared.system.then_some(who.uid),
                    add: to_path(add),
                    remove: to_path(remove),
                    reply,
                });
                match asked.ok().and_then(|()| answer.recv().ok()) {
                    Some(folders) => Answer::Excluded {
                        folders: folders.iter().map(|p| WirePath::of(p)).collect(),
                    },
                    None => failure("internal", "the engine is going"),
                }
            }
            ask @ Ask::Volumes { .. } => volumes(shared, commands, &client, ask, who),
            Ask::Watch { add, remove } => {
                let (reply, answer) = channel();
                let to_path = |v: Vec<WirePath>| v.iter().map(WirePath::to_path).collect();
                let asked = commands.send(Command::Watch {
                    client: client.clone(),
                    add: to_path(add),
                    remove: to_path(remove),
                    reply,
                });
                match asked.ok().and_then(|()| answer.recv().ok()) {
                    Some(watching) => Answer::Watching(watching),
                    None => failure("internal", "the engine is going"),
                }
            }
        };
        if !send(&writer, id, answer) {
            break;
        }
    }
    for q in queries {
        let _ = q.join();
    }
}

/// `watch` at the system service, which hears every folder already:
/// nothing kept, no share, and each of `folders` said as its coverage is,
/// to this asker.
fn watching_all(shared: &Shared, folders: Vec<WirePath>, asker: &Asker) -> Answer {
    let kept = |path: WirePath| {
        let state = match coverage(shared, &path, asker).map(|c| c.names) {
            Ok(wire::Proof::Live(since)) => wire::Keeping::Live(since),
            Ok(wire::Proof::NotCovered(why)) => {
                wire::Keeping::NotLive(wire::NotLive::NotCovered(why))
            }
            Ok(_) => wire::Keeping::NotLive(wire::NotLive::Pending),
            Err(_) => wire::Keeping::NotLive(wire::NotLive::Missing),
        };
        wire::Kept {
            path,
            state,
            folders: None,
            within: None,
        }
    };
    Answer::Watching(wire::Watching {
        folders: folders.into_iter().map(kept).collect(),
        share: None,
    })
}

fn sync(shared: &Shared, commands: &Sender<Command>, root: &WirePath, asker: &Asker) -> Answer {
    let Some((i, rel)) = shared.locate(&root.to_path()) else {
        return Answer::Coverage(outside());
    };
    let (reply, done) = channel();
    if commands
        .send(Command::Sync {
            root: i,
            rel,
            reply,
        })
        .is_err()
        || done.recv().is_err()
    {
        return failure("internal", "the engine is going");
    }
    match coverage(shared, root, asker) {
        Ok(c) => Answer::Coverage(c),
        Err(a) => a,
    }
}

fn changes(
    shared: &Shared,
    commands: &Sender<Command>,
    root: &WirePath,
    since: String,
    who: &Asker,
) -> Answer {
    let Some((i, rel)) = shared.locate(&root.to_path()) else {
        return Answer::TooOld {};
    };
    let (reply, answer) = channel();
    let asked = commands.send(Command::Changes {
        root: i,
        rel,
        since,
        reply,
    });
    let Some(answer) = asked.ok().and_then(|()| answer.recv().ok()) else {
        return failure("internal", "the engine is going");
    };
    let Ok((events, token, complete)) = answer else {
        return Answer::TooOld {};
    };
    let base = &shared.root(i);
    // Another user than root, asking the system service, is told what
    // changed in folders they may list now (DESIGN 4): an event is theirs
    // if they may list the folder holding what it names. A removal in a
    // folder removed since is told by the topmost removal, in a folder
    // still there.
    let index = match shared.snapshot(i) {
        Published::Ready(index) => Some(index),
        _ => None,
    };
    let seen = |path: &Path| -> bool {
        let Some(index) = &index else {
            return !shared.system || who.is_root();
        };
        let Some(s) = seeing(shared, i, index, who) else {
            return true;
        };
        let parent = path.parent().unwrap_or(Path::new(""));
        matches!(reached(&s, parent, &mut HashMap::new()), Reached::Listed(_))
    };
    let events = events
        .iter()
        .filter(|e| match e {
            Event::Entry { path, .. } | Event::NotCovered { path, .. } => seen(path),
        })
        .map(|e| match e {
            Event::Entry { path, op, entry } => {
                let entry = convert::entry(&base.join(path), entry);
                match op {
                    skwird_index::log::Op::Added => wire::Event::Added(entry),
                    skwird_index::log::Op::Removed => wire::Event::Removed(entry),
                    skwird_index::log::Op::Changed => wire::Event::Changed(entry),
                }
            }
            Event::NotCovered { path, why } => wire::Event::NotCovered {
                path: WirePath::of(&base.join(path)),
                why: convert::why(*why),
            },
        })
        .collect();
    // The folders not covered, whose changes no event holds: as the root's
    // coverage names them to this asker now.
    let holes = coverage(shared, root, who).map_or_else(|_| Vec::new(), |c| c.holes);
    Answer::Changes(wire::Changes {
        events,
        token: token.text(),
        complete: wire::Complete {
            names: complete.names,
            attrs: complete.attrs,
        },
        holes,
    })
}

/// Answer a query from the snapshots published now, in batches, then
/// `done` with the coverage that held; cancelled, `done` says so. The
/// roots below the folder asked (the system service's other filesystems)
/// answer too, each at its place, so that a query of `/` holds `/home`.
fn answer_query(
    shared: &Shared,
    writer: &Writer,
    id: u64,
    query: &wire::Query,
    cancelled: &AtomicBool,
    who: &Asker,
) -> Answer {
    let path = query.root.to_path();
    let done = |coverage, count, truncated| {
        Answer::Done(wire::Done {
            coverage,
            count,
            truncated,
            cancelled: cancelled.load(Ordering::Relaxed),
        })
    };
    let Some((i, rel)) = shared.locate(&path) else {
        return done(outside(), 0, false);
    };
    // The system service answers any local user: a query gives at most
    // so many results, past which its `done` says there were more.
    let most = match (query.limit, shared.system) {
        (Some(l), true) => Some(l.min(MOST_RESULTS)),
        (None, true) => Some(MOST_RESULTS),
        (l, false) => l,
    };
    let limit = most.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let asked = Query {
        name: query.name.as_ref().map(|n| NameQuery {
            pattern: match n.how {
                wire::Match::Skwir => Pattern::Skwir(n.text.clone()),
                wire::Match::Substring => Pattern::Substring(n.text.clone()),
                wire::Match::Glob => Pattern::Glob(n.text.clone()),
                wire::Match::Exact => Pattern::Exact(n.text.clone()),
            },
            case_sensitive: n.case_sensitive,
        }),
        kinds: query
            .kinds
            .as_ref()
            .map(|ks| ks.iter().map(|&k| convert::kind_of(k)).collect()),
        size: query.size.map(|[a, b]| a..=b),
        mtime: query
            .mtime
            .map(|[a, b]| convert::time_of(a)..=convert::time_of(b)),
        // One more than asked, to say whether there were more.
        limit: Some(limit.saturating_add(1)),
        unfiltered_links: query.links == wire::Links::Unfiltered,
    };
    let (mut entries, mut coverage) = match answer_in(shared, i, &rel, &asked, who) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    if !matches!(coverage.names, wire::Proof::NotCovered(_)) {
        let mut below = shared.roots_below(&path, i);
        below.sort_by_key(|&j| shared.root(j).components().count());
        for j in below {
            if entries.len() > limit {
                break;
            }
            let Ok((more, inner)) = answer_in(shared, j, Path::new(""), &asked, who) else {
                continue;
            };
            coverage = at_root_below(coverage, &shared.root(j), inner);
            entries.extend(more);
        }
    }
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    let mut sent = 0u64;
    for batch in entries.chunks(BATCH) {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        if !send(writer, id, Answer::Results(batch.to_vec())) {
            break;
        }
        sent += batch.len() as u64;
    }
    done(coverage, sent, truncated)
}

/// `outer`'s coverage with root `at`'s, `inner`, at its place: merged if
/// it is covered; otherwise (the asker may not reach it, or it is not
/// read yet) a hole there, the rest as it was.
fn at_root_below(outer: wire::Coverage, at: &Path, inner: wire::Coverage) -> wire::Coverage {
    match inner.names {
        wire::Proof::NotCovered(why) => {
            let mut outer = outer;
            let path = WirePath::of(at);
            outer.holes.retain(|h| h.path != path);
            outer.holes.push(wire::Hole { path, why });
            outer
        }
        _ => crate::coverage::with_root_below(outer, at, inner),
    }
}

/// What root `i` answers below `rel` to `asked`, as `who` may see it, and
/// the coverage that held.
fn answer_in(
    shared: &Shared,
    i: usize,
    rel: &Path,
    asked: &Query,
    who: &Asker,
) -> Result<(Vec<wire::Entry>, wire::Coverage), Answer> {
    let index = match shared.snapshot(i) {
        Published::Ready(index) => index,
        Published::Opening => {
            return Ok((
                Vec::new(),
                wire::Coverage {
                    names: wire::Proof::NotCovered(wire::Why::Unread),
                    attrs: wire::Proof::NotCovered(wire::Why::Unread),
                    holes: Vec::new(),
                },
            ));
        }
        Published::Failed(why) => return Err(failure("internal", why)),
    };
    // Another user than root, asking the system service, is answered what
    // they could list (DESIGN 4).
    let seeing = seeing(shared, i, &index, who);
    if let Some(s) = &seeing
        && let Reached::Denied = reached(s, rel, &mut HashMap::new())
    {
        return Ok((Vec::new(), denied()));
    }
    let not_a_folder = || failure("not_a_folder", "no folder of the index is at that path");
    let mut coverage = coverage_at(&index, &shared.root(i), rel).ok_or_else(not_a_folder)?;
    let folder = index.folder_at(rel).ok_or_else(not_a_folder)?;
    let found = match &seeing {
        None => index.query(folder, asked).map(|hits| (hits, Vec::new())),
        Some(s) => {
            let sight = |f: FolderId, memo: &mut HashMap<FolderId, Sight>| s.sight(f, memo);
            index.query_seen(folder, asked, Some(&sight))
        }
    };
    let (hits, _) = found.map_err(|e| failure("bad_pattern", e))?;
    // A hole is a folder's name: said only to who may list its parent. A
    // folder whose ACL was not read is not covered, so that the client
    // walks it, whether anything in it answered or not: whether a name is
    // there is not said either.
    if let Some(s) = &seeing {
        let mut memo = HashMap::new();
        s.seen_holes(&shared.root(i), &mut coverage.holes, &mut memo);
        let holes = s.acl_holes(folder, &mut memo);
        coverage
            .holes
            .extend(holes.into_iter().map(|f| hole(shared, i, &index, f)));
    }
    let root: &Path = &shared.root(i);
    Ok((
        hits.iter().map(|h| convert::hit(root, h)).collect(),
        coverage,
    ))
}

/// A thread started under `name`, which the system shows for it (in
/// `/proc`, `top -H`, a profiler's samples): at most 15 bytes on Linux.
pub(crate) fn named<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .expect("a thread starts, as std::thread::spawn would have it")
}

/// What this engine says it does beyond its version (`Welcome::features`):
/// names in both forms everywhere; removable volumes at a user's engine
/// where its system's are seen (`skwird_crawl::removable`), Linux's and
/// macOS' so far.
fn features(system: bool) -> Vec<String> {
    let mut said = vec![wire::NAMES_BOTH_FORMS.to_owned()];
    if !system && cfg!(any(target_os = "linux", target_os = "macos")) {
        said.push(wire::VOLUMES.to_owned());
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At the system service, a user other than root asks the writer for
    /// work once a second at most; root, and a user's engine, at will.
    #[test]
    fn a_user_asks_the_writer_once_a_second() {
        let shared = Shared::new(Vec::new(), true);
        let someone = Asker {
            uid: 1000,
            gid: 1000,
            groups: Vec::new(),
            token: None,
        };
        let root = Asker {
            uid: 0,
            gid: 0,
            groups: Vec::new(),
            token: None,
        };
        assert!(!too_soon(&shared, &someone));
        assert!(too_soon(&shared, &someone), "again at once");
        // Root as Unix names it; on Windows, LocalSystem's token alone.
        #[cfg(unix)]
        assert!(!too_soon(&shared, &root) && !too_soon(&shared, &root));
        let _ = root;
        let user = Shared::new(Vec::new(), false);
        assert!(!too_soon(&user, &someone) && !too_soon(&user, &someone));
    }

    /// At the system service, a user runs at most so many queries at once,
    /// another user's not counted; one ended gives its place back. A user's
    /// engine counts none.
    #[test]
    fn a_user_runs_so_many_queries_at_once() {
        let shared = Arc::new(Shared::new(Vec::new(), true));
        let held: Vec<Running> = (0..MOST_QUERIES)
            .map(|_| Running::query(&shared, 1000).unwrap())
            .collect();
        assert!(Running::query(&shared, 1000).is_none(), "one more refused");
        assert!(Running::query(&shared, 1001).is_some(), "another user's");
        drop(held);
        assert!(Running::query(&shared, 1000).is_some(), "given back");
        let open: Vec<Held> = (0..MOST_CONNECTIONS)
            .map(|_| Running::connection(&shared, 1000).unwrap())
            .collect();
        assert!(
            Running::connection(&shared, 1000).is_none(),
            "one more refused"
        );
        assert!(Running::query(&shared, 1000).is_some(), "counted apart");
        drop(open);
        let user = Arc::new(Shared::new(Vec::new(), false));
        let many: Vec<Running> = (0..10)
            .map(|_| Running::query(&user, 1000).unwrap())
            .collect();
        assert_eq!(many.len(), 10);
    }
}
