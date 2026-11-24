//! Live folders in the engine (`docs/PLAN.md`, phase 2, step 3): what the
//! watcher hears is sent to the writer, which gathers it a folder at a
//! time and reads each folder again once its gathering is done (DESIGN
//! 8). The writer waits with a deadline only while something is due: a
//! gathering, the check after an overflow, a save. Otherwise it waits
//! for a command alone: idle is idle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use skwird_crawl::Watcher;
#[cfg(target_os = "linux")]
use skwird_crawl::fanotify;
use skwird_crawl::fanotify::Group;
use skwird_index::block::FolderId;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use crate::engine::Command;
use crate::engine::log;

/// A folder's events are gathered until this passes without one...
const QUIET: Duration = Duration::from_millis(100);
/// ...or this long after the first.
const MOST: Duration = Duration::from_secs(1);
/// After a read that gave events, the reader waits this long before the
/// next. Woken for each event, the writer spent more on waking than on
/// what it was woken for; woken every 10 ms through a storm, each wake
/// still cost what its fixed part does (DESIGN 8, "A wake for many
/// events"). Half of [`QUIET`], it ends no gathering later than that; a
/// sync's cookie wakes it at once. Idle, the reader waits for the next
/// event, as before.
#[cfg(target_os = "linux")]
const READ_EVERY: Duration = Duration::from_millis(50);
/// What was heard is saved this long after the first change unsaved: the
/// log keeps it durable meanwhile.
const SAVE: Duration = Duration::from_secs(10);
/// After an overflow, the check waits for this long without an event,
/// doubling, up to the next, for each overflow that follows soon.
const BACKOFF: Duration = Duration::from_secs(2);
const BACKOFF_MOST: Duration = Duration::from_secs(60);
/// An overflow this long after the last check starts the wait afresh.
const CALM: Duration = Duration::from_secs(300);
/// A folder's own pace: applied alone again and again, it takes at most one
/// part in this many of the time (1%). Folders whose reads their share
/// limits so divide one part in `2 * DUTY` (0.5%) between them
/// ([`Gather::applied`]).
const DUTY: u32 = 100;
/// All the writer does for what is heard (finding each folder, applying,
/// saving, a check after an overflow) goes through one account, which
/// fills at this many parts in `2 * DUTY` of the time (1.5%), whatever the
/// folders do ([`Gather::spent`]): three quarters of the budget (DESIGN
/// 10), reading the events, which cannot wait without being lost, keeping
/// the rest. The folders a share limits take a third of it at most, so
/// that the others keep two.
const ACCOUNT: u32 = 3;
/// What the account lets through at once after a quiet time, and the most
/// it is owed: what it fills in its window ([`window`]).
const BURST: Duration = Duration::from_millis(150);
/// The most folders whose last cost is kept: past them, every one is let
/// go of, and taken to cost what a folder typically does.
const COSTS_KEPT: usize = 4096;
/// How fast what a folder used is forgotten: by e every this long. A folder
/// busy for a while goes after a quiet one, and its use decays while it
/// waits, so that each busy one takes its turn.
const USE_DECAY: Duration = Duration::from_secs(60);

/// The time the account takes to fill with `work`.
fn at_share(work: Duration) -> Duration {
    work * (2 * DUTY) / ACCOUNT
}

/// The account's window: what it takes to fill, from empty to [`BURST`].
fn window() -> Duration {
    at_share(BURST)
}

/// What the writer listens to: a user's engine's watcher (inotify), or
/// the system service's group (fanotify).
#[cfg(target_os = "linux")]
pub trait Source {
    type Heard;
    fn fd(&self) -> rustix::fd::BorrowedFd<'_>;
    fn generation(&self) -> u64;
    fn read(&self, heard: &mut Vec<Self::Heard>) -> std::io::Result<()>;
    /// What a failed read is said as: nothing heard since proves anything.
    fn overflow() -> Self::Heard;
    fn command(generation: u64, heard: Vec<Self::Heard>) -> Command;
}

#[cfg(target_os = "linux")]
impl Source for Watcher {
    type Heard = skwird_crawl::Heard;
    fn fd(&self) -> rustix::fd::BorrowedFd<'_> {
        Watcher::fd(self)
    }
    fn generation(&self) -> u64 {
        Watcher::generation(self)
    }
    fn read(&self, heard: &mut Vec<Self::Heard>) -> std::io::Result<()> {
        Watcher::read(self, heard)
    }
    fn overflow() -> Self::Heard {
        skwird_crawl::Heard::Overflow
    }
    fn command(generation: u64, heard: Vec<Self::Heard>) -> Command {
        Command::Heard { generation, heard }
    }
}

#[cfg(target_os = "linux")]
impl Source for Group {
    type Heard = fanotify::Heard;
    fn fd(&self) -> rustix::fd::BorrowedFd<'_> {
        Group::fd(self)
    }
    fn generation(&self) -> u64 {
        Group::generation(self)
    }
    fn read(&self, heard: &mut Vec<Self::Heard>) -> std::io::Result<()> {
        Group::read(self, heard)
    }
    fn overflow() -> Self::Heard {
        fanotify::Heard::Overflow
    }
    fn command(generation: u64, heard: Vec<Self::Heard>) -> Command {
        Command::Marks { generation, heard }
    }
}

/// Listen to Windows' watcher for the writer (phase 4, step 5): wait until
/// a root's thread heard something, and send it all. Each root is heard on
/// a thread of its own; this one gathers them for the writer. On macOS, the
/// roots' stream is heard so (phase 5, step 3): what it gives is already
/// a batch, held back by its latency.
#[cfg(any(windows, target_os = "macos"))]
pub fn listen(watcher: &Watcher, commands: &Sender<Command>, readers: &Readers) {
    readers.join();
    loop {
        let mut heard = Vec::new();
        if let Err(e) = watcher.read(&mut heard) {
            log(&format!("cannot read the watcher: {e}"));
            return;
        }
        let generation = watcher.generation();
        if commands.send(Command::Heard { generation, heard }).is_err() {
            return;
        }
    }
}

/// Listen to `source` for the writer: block until it can be read, send
/// what it heard, and again. A read that fails is said as an overflow:
/// nothing heard since proves anything. The mount table is followed too,
/// as skwir follows it: `/proc/self/mountinfo` signals a change with
/// `POLLPRI`, and is read through so that it signals the next.
#[cfg(target_os = "linux")]
pub fn listen<S: Source>(source: &S, commands: &Sender<Command>, readers: &Readers) {
    use std::io::{Read, Seek};
    readers.join();
    let mut heard = Vec::new();
    let mut table = std::fs::File::open("/proc/self/mountinfo").ok();
    let mut skip = Vec::new();
    if let Some(t) = table.as_mut() {
        let _ = t.read_to_end(&mut skip);
    }
    loop {
        let fd = source.fd();
        let mut fds = vec![rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        if let Some(t) = &table {
            fds.push(rustix::event::PollFd::new(t, rustix::event::PollFlags::PRI));
        }
        match rustix::event::poll(&mut fds, None) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => {
                log(&format!("cannot wait on the watcher: {e}"));
                return;
            }
        }
        let mounts = fds.get(1).is_some_and(|f| !f.revents().is_empty());
        drop(fds);
        if mounts && let Some(t) = table.as_mut() {
            skip.clear();
            let read = t
                .rewind()
                .and_then(|()| t.read_to_end(&mut skip).map(|_| ()));
            if read.is_err() {
                log("cannot read the mount table again: it is not followed");
                table = None;
            }
            if commands.send(Command::Mounts).is_err() {
                return;
            }
        }
        if let Err(e) = source.read(&mut heard) {
            log(&format!("cannot read what was heard: {e}"));
            heard.push(S::overflow());
        }
        if heard.is_empty() {
            continue;
        }
        let command = S::command(source.generation(), std::mem::take(&mut heard));
        if commands.send(command).is_err() {
            return;
        }
        // What comes meanwhile queues in the kernel, read at once after:
        // the writer wakes for a batch, not for each event. Not while a
        // sync waits for its cookie, and woken as one is made.
        if !readers.awaited() {
            std::thread::park_timeout(READ_EVERY);
        }
    }
}

/// What the writer tells the readers ([`listen`]): whether a sync waits for
/// its cookie, so that no reader waits to batch what it hears meanwhile,
/// and the readers' threads, to wake one waiting as a cookie is made.
#[derive(Default)]
pub struct Readers {
    awaited: std::sync::atomic::AtomicBool,
    threads: std::sync::Mutex<Vec<std::thread::Thread>>,
}

impl Readers {
    fn join(&self) {
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(std::thread::current());
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn awaited(&self) -> bool {
        self.awaited.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether a cookie is awaited: derived by the writer, after each of
    /// its turns, from the syncs waiting.
    pub fn set_awaited(&self, yes: bool) {
        self.awaited
            .store(yes, std::sync::atomic::Ordering::Release);
    }

    /// A cookie was made: awaited, and every reader woken, so that none
    /// waits to read it.
    pub fn wake(&self) {
        self.set_awaited(true);
        for t in self
            .threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            t.unpark();
        }
    }
}

pub use skwird_index::update::HeardFolder;

/// Past this many names, a folder is read whole: what is kept of a
/// gathering stays bounded.
const NAMES: usize = 4096;

/// A folder's gathering: when first heard, when last, the names heard,
/// and what finding it took, charged to its application.
struct Gathered {
    first: Instant,
    last: Instant,
    names: Option<HashSet<Box<[u8]>>>,
    owed: Duration,
}

/// Until when a folder applied is not applied again, and whether its
/// share holds it longer than a gathering's most: such folders divide one
/// part in `2 * DUTY` between them.
#[derive(Clone, Copy)]
struct Hold {
    until: Instant,
    limited: bool,
}

/// What the writer has heard and not applied, and what is due after.
#[derive(Default)]
pub struct Gather {
    /// Each folder heard, by root.
    heard: HashMap<(usize, FolderId), Gathered>,
    /// When the check after an overflow is due.
    recheck: Option<Instant>,
    backoff: Duration,
    last_check: Option<Instant>,
    /// Since when what was heard has not been saved.
    unsaved: Option<Instant>,
    /// Until when no save is due: what the last took, `DUTY` - 1 times over.
    save_held: Option<Instant>,
    /// Until when each folder applied is not applied again: its share of
    /// what its reads took, `DUTY` - 1 times over and more.
    holds: HashMap<(usize, FolderId), Hold>,
    /// What finding each folder due took, until it is applied.
    owed: HashMap<(usize, FolderId), Duration>,
    /// Until when the writer's work for what is heard takes its share:
    /// nothing due is applied before, but one due for longer than a window.
    account: Option<Instant>,
    /// When a folder was last taken while the account was owed.
    forced: Option<Instant>,
    /// What [`Gather::spent`] charged since the writer's last turn.
    charged: Duration,
    /// What each folder applied lately was charged: what it is taken to
    /// cost next. At most [`COSTS_KEPT`].
    costs: HashMap<(usize, FolderId), Duration>,
    /// The mean of what the last application charged a folder.
    typical: Option<Duration>,
    /// What each folder applied lately cost, decaying by [`USE_DECAY`],
    /// as of when: the least used go first. At most [`COSTS_KEPT`].
    used: HashMap<(usize, FolderId), (f64, Instant)>,
    /// A time before which no folder heard is ready ([`Gather::ready`]),
    /// and one before which none may be taken while the account is owed:
    /// none while none is heard. Bounds, kept as folders are heard and
    /// found again only where they pass, not at each of the writer's
    /// turns: a turn went over every folder waiting, which a busy machine
    /// made many, and was charged to the account it held owed. What is
    /// heard of a folder, or a hold, only makes it later; a folder new is
    /// ready a gathering's quiet after it at the soonest.
    soonest: Option<Instant>,
    forced_at: Option<Instant>,
}

impl Gather {
    /// A change heard in `folder` of root `root`, of the folder itself:
    /// it is to be read whole.
    pub fn note(&mut self, root: usize, folder: FolderId, now: Instant) {
        self.gathered(root, folder, now).0.names = None;
    }

    /// A change heard in the entry `name` of `folder`: of what the folder
    /// held, that entry alone is to be stat-ed again.
    pub fn note_entry(&mut self, root: usize, folder: FolderId, name: Box<[u8]>, now: Instant) {
        let (g, fresh) = self.gathered(root, folder, now);
        if fresh {
            g.names = Some(HashSet::new());
        }
        if let Some(names) = g.names.as_mut() {
            names.insert(name);
            if names.len() > NAMES {
                g.names = None;
            }
        }
    }

    /// `folder` of `root` heard at `now`, and whether it is new: one look
    /// for each event.
    fn gathered(&mut self, root: usize, folder: FolderId, now: Instant) -> (&mut Gathered, bool) {
        // The check after an overflow waits for the queue to be quiet.
        if let Some(at) = self.recheck.as_mut() {
            *at = (*at).max(now + self.backoff);
        }
        match self.heard.entry((root, folder)) {
            std::collections::hash_map::Entry::Occupied(e) => {
                let g = e.into_mut();
                g.last = now;
                (g, false)
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                let at = now + QUIET;
                self.soonest = Some(self.soonest.map_or(at, |s| s.min(at)));
                let forced = at + window();
                self.forced_at = Some(self.forced_at.map_or(forced, |f| f.min(forced)));
                let g = v.insert(Gathered {
                    first: now,
                    last: now,
                    names: None,
                    owed: Duration::ZERO,
                });
                (g, true)
            }
        }
    }

    /// Finding `folder` of root `root`, heard, took `took` of the writer's
    /// processor time: charged to its next application.
    pub fn found(&mut self, root: usize, folder: FolderId, took: Duration) {
        if let Some(g) = self.heard.get_mut(&(root, folder)) {
            g.owed += took;
        }
    }

    /// The writer spent `took` of processor time on what was heard: finding
    /// folders, applying them, saving, a check after an overflow. Charged
    /// to the account, which fills at [`ACCOUNT`] parts in `2 * DUTY` of
    /// the time. It lets [`BURST`] through at once after a
    /// quiet time, and is owed at most as much, so that no burst holds what
    /// follows for longer than its window.
    pub fn spent(&mut self, took: Duration, now: Instant) {
        self.charge(took, now);
        self.charged += took;
    }

    /// The writer's turn took `whole` of its processor time, but what a
    /// client asked for: what [`Gather::spent`] was not told of (waking,
    /// the deadline, the bookkeeping) is charged too, since a share counts
    /// all the work it bounds.
    pub fn turned(&mut self, whole: Duration, now: Instant) {
        let rest = whole.saturating_sub(std::mem::take(&mut self.charged));
        self.charge(rest, now);
    }

    fn charge(&mut self, took: Duration, now: Instant) {
        let floor = now.checked_sub(window()).unwrap_or(now);
        let from = self.account.map_or(floor, |at| at.max(floor));
        self.account = Some((from + at_share(took)).min(now + window()));
    }

    /// Whether the account lets what is due be applied at `now`.
    fn open(&self, now: Instant) -> bool {
        self.account.is_none_or(|at| at <= now)
    }

    /// When the gathering `g` of `key` is done and its hold past.
    fn ready(&self, key: &(usize, FolderId), g: &Gathered) -> Instant {
        let at = (g.last + QUIET).min(g.first + MOST);
        // Held after its last application, a folder waits for it.
        self.holds.get(key).map_or(at, |hold| at.max(hold.until))
    }

    /// The folders due longest, their gathering done and their hold past,
    /// with their root, let go of here. The least used lately go first
    /// ([`USE_DECAY`]), so that a quiet folder's change waits for no round
    /// of the busy ones, and then the longest due. Of the root of the
    /// first, in that order, those taken while what each cost when last
    /// applied (or what a folder typically costs) fits what the account
    /// holds, and that one at least. One batch shares what an application
    /// costs whatever it holds (the log made durable, the index
    /// published); each is charged before the next is taken, so that the
    /// account is overshot by little. While the account is owed, one
    /// folder due for longer than its window is taken all the same, one a
    /// window at most: what cannot wait (finding what is heard) may
    /// overdraw it for good, and nothing then waits on it for good.
    pub fn due(&mut self, now: Instant) -> Option<(usize, Vec<HeardFolder>)> {
        // Before the bounds, nothing can be taken: found out without going
        // over what waits.
        let earliest = if self.open(now) {
            self.soonest
        } else {
            self.forced_at
        };
        if earliest.is_none_or(|at| now < at) {
            return None;
        }
        let taken = self.take_due(now);
        self.bound();
        taken
    }

    /// The bounds found again from what waits ([`Gather::soonest`]).
    fn bound(&mut self) {
        let soonest = self.heard.iter().map(|(key, g)| self.ready(key, g)).min();
        self.soonest = soonest;
        self.forced_at = soonest.map(|at| {
            self.forced
                .map_or(at + window(), |last| (last + window()).max(at + window()))
        });
    }

    /// What [`Gather::due`] takes, going over what waits.
    fn take_due(&mut self, now: Instant) -> Option<(usize, Vec<HeardFolder>)> {
        self.holds.retain(|_, hold| now < hold.until);
        let mut ready: Vec<(u64, Instant, (usize, FolderId))> = self
            .heard
            .iter()
            .map(|(key, g)| (self.ready(key, g), *key))
            .filter(|&(at, _)| at <= now)
            .map(|(at, key)| (self.use_of(key, now), at, key))
            .collect();
        ready.sort_unstable();
        let &(_, _, (root, _)) = ready.first()?;
        let &(_, at, oldest) = ready.iter().min_by_key(|(_, at, key)| (*at, *key))?;
        let mut taken = Vec::new();
        if self.open(now) {
            // What the account holds now, as work: [`BURST`] at most.
            let holds = self.account.map_or(BURST, |owed| {
                (now.saturating_duration_since(owed) * ACCOUNT / (2 * DUTY)).min(BURST)
            });
            let mut fits = Duration::ZERO;
            for &(_, _, key) in ready.iter().filter(|(_, _, key)| key.0 == root) {
                let cost = self.costs.get(&key).copied().unwrap_or(self.typical());
                if !taken.is_empty() && fits + cost > holds {
                    break;
                }
                fits += cost;
                taken.push(key);
            }
        } else {
            let forced = self.forced.is_none_or(|last| last + window() <= now);
            if now < at + window() || !forced {
                return None;
            }
            self.forced = Some(now);
            taken.push(oldest);
        }
        let mut folders = Vec::with_capacity(taken.len());
        for key in taken {
            if let Some(g) = self.heard.remove(&key) {
                if !g.owed.is_zero() {
                    self.owed.insert(key, g.owed);
                }
                folders.push((key.1, g.names));
            }
        }
        Some((root, folders))
    }

    /// What `key` used lately, in nanoseconds, decayed to `now`.
    fn use_of(&self, key: (usize, FolderId), now: Instant) -> u64 {
        self.used.get(&key).map_or(0, |&(used, at)| {
            let age = now.saturating_duration_since(at).as_secs_f64();
            let decayed = used * (-age / USE_DECAY.as_secs_f64()).exp();
            // Nanoseconds of a few seconds' work at most: no truncation.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let nanos = (decayed * 1e9) as u64;
            nanos
        })
    }

    /// What a folder not applied lately is taken to cost: the mean of what
    /// the last application charged, 1 ms before any.
    fn typical(&self) -> Duration {
        self.typical.unwrap_or(Duration::from_millis(1))
    }

    /// The folders of `root` read, each with the entries its listing held,
    /// took `took` of processor time. Each is held, before it is applied
    /// again, for its share of that time (by its entries) and of what
    /// finding it took, [`DUTY`] - 1 times over: a large folder heard
    /// without pause waits for itself, and a small one read with it does
    /// not. A folder held longer than a gathering's most is limited by the
    /// share; such folders divide one part in `2 * DUTY`, each held as many
    /// times longer as there are others, however many are busy: a third of
    /// the account at most, which all of `took` is charged to
    /// ([`Gather::spent`]). Their answers lag then, live still; a sync
    /// applies them at once.
    pub fn applied(
        &mut self,
        root: usize,
        read: &[(FolderId, usize)],
        took: Duration,
        now: Instant,
    ) {
        let owed: Vec<Duration> = read
            .iter()
            .map(|(folder, _)| self.owed.remove(&(root, *folder)).unwrap_or_default())
            .collect();
        // What was due and not read (gone since) owes nothing.
        self.owed.retain(|(r, _), _| *r != root);
        let total: usize = read.iter().map(|(_, n)| n + 1).sum();
        if total == 0 {
            self.spent(took, now);
            return;
        }
        self.holds.retain(|_, hold| now < hold.until);
        let limited = self.holds.values().filter(|hold| hold.limited).count();
        let sharing = u32::try_from(limited + 1).unwrap_or(u32::MAX);
        for (&(folder, n), owed) in read.iter().zip(owed) {
            let applying = took.mul_f64((n + 1) as f64 / total as f64);
            let share = applying + owed;
            let alone = share * (DUTY - 1);
            let limits = alone > MOST;
            let wait = if limits {
                share * (2 * DUTY).saturating_mul(sharing).saturating_sub(1)
            } else {
                alone
            };
            if self.costs.len() >= COSTS_KEPT {
                self.costs.clear();
            }
            self.costs.insert((root, folder), applying);
            if self.used.len() >= COSTS_KEPT {
                self.used.clear();
            }
            let used = self.use_of((root, folder), now) as f64 / 1e9 + share.as_secs_f64();
            self.used.insert((root, folder), (used, now));
            if wait >= Duration::from_millis(1) {
                self.holds.insert(
                    (root, folder),
                    Hold {
                        until: now + wait,
                        limited: limits,
                    },
                );
            }
        }
        let count = u32::try_from(read.len()).unwrap_or(u32::MAX);
        self.typical = Some(took / count);
        self.spent(took, now);
    }

    /// Whether a folder of `root` heard is still to be applied.
    pub fn holds(&self, root: usize) -> bool {
        self.heard.keys().any(|(r, _)| *r == root)
    }

    /// Every folder of `root` heard, done gathering or not: a sync takes
    /// them at once.
    pub fn take(&mut self, root: usize) -> Vec<HeardFolder> {
        let keys: Vec<(usize, FolderId)> = self
            .heard
            .keys()
            .filter(|(r, _)| *r == root)
            .copied()
            .collect();
        // Asked for, not held: what finding them took is not owed.
        self.owed.retain(|(r, _), _| *r != root);
        let mut folders: Vec<HeardFolder> = keys
            .into_iter()
            .filter_map(|key| self.heard.remove(&key).map(|g| (key.1, g.names)))
            .collect();
        folders.sort_unstable_by_key(|(folder, _)| *folder);
        self.bound();
        folders
    }

    /// Events were dropped: the check is due once the queue is quiet, the
    /// wait doubling while overflows follow one another.
    pub fn overflowed(&mut self, now: Instant) {
        let soon = self.last_check.is_some_and(|at| now < at + CALM);
        self.backoff = if soon && !self.backoff.is_zero() {
            (self.backoff * 2).min(BACKOFF_MOST)
        } else {
            BACKOFF
        };
        self.recheck = Some(now + self.backoff);
    }

    /// Whether the check after an overflow is due now; once said, it is
    /// not due again until the next overflow.
    pub fn recheck_due(&mut self, now: Instant) -> bool {
        if self.recheck.is_some_and(|at| now >= at) {
            self.recheck = None;
            self.last_check = Some(now);
            return true;
        }
        false
    }

    /// A sync ran the check: none is due for an overflow before it.
    pub fn checked(&mut self, now: Instant) {
        self.recheck = None;
        self.last_check = Some(now);
    }

    /// What was heard changed an index: it is to be saved.
    pub fn changed(&mut self, now: Instant) {
        self.unsaved.get_or_insert(now);
    }

    /// Whether the indexes changed by what was heard are to be saved now:
    /// [`SAVE`] after the first change unsaved, and not before the last
    /// save's hold is past.
    pub fn save_due(&mut self, now: Instant) -> bool {
        if self.save_at().is_some_and(|at| now >= at) {
            self.unsaved = None;
            return true;
        }
        false
    }

    /// The save took `took` of processor time: the next waits for its
    /// share of it, as a folder's application does, and the account is
    /// charged with it. A save is never held by the account: what it costs
    /// holds what is due after it.
    pub fn saved(&mut self, took: Duration, now: Instant) {
        self.save_held = Some(now + took * (DUTY - 1));
        self.spent(took, now);
    }

    fn save_at(&self) -> Option<Instant> {
        let at = self.unsaved? + SAVE;
        Some(self.save_held.map_or(at, |held| at.max(held)))
    }

    /// When a folder heard may be due next, from the bounds
    /// ([`Gather::soonest`]): never later than one is, so that what wakes
    /// early finds the bounds again.
    fn gathering(&self) -> Option<Instant> {
        self.soonest.map(|at| match self.account {
            None => at,
            Some(open) => at.max(open).min(self.forced_at.unwrap_or(at + window())),
        })
    }

    /// When something is due next, if anything is.
    pub fn deadline(&self) -> Option<Instant> {
        // What is due waits for the account too, or to be due a window,
        // a window after the last taken so.
        [self.gathering(), self.recheck, self.save_at()]
            .into_iter()
            .flatten()
            .min()
    }
}

/// The watcher an engine listens to, if inotify gives one: `budget`
/// watches at most, the user's share by default (`SKWIRD_WATCHES` sets
/// it, for tests). None, the engine answers checked, never live.
pub fn watcher(budget: Option<usize>) -> Option<Arc<Watcher>> {
    let budget = budget.unwrap_or_else(Watcher::share);
    match Watcher::new(1, budget) {
        Ok(w) => Some(Arc::new(w)),
        Err(e) => {
            log(&format!(
                "cannot watch, answers are checked, never live: {e}"
            ));
            None
        }
    }
}

/// The system service's group, if fanotify gives one: marks whole
/// filesystems, which needs `CAP_SYS_ADMIN`. None, the service answers
/// checked, never live.
pub fn group() -> Option<Arc<Group>> {
    match Group::new(1) {
        Ok(g) => Some(Arc::new(g)),
        Err(e) => {
            log(&format!(
                "cannot mark filesystems, answers are checked, never live: {e}"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder's gathering is done after a quiet time, or at most a time
    /// after its first change, whichever comes first.
    #[test]
    fn a_gathering_ends_after_quiet_or_at_most() {
        let t = Instant::now();
        let mut g = Gather::default();
        g.note(0, 7, t);
        assert_eq!(g.deadline(), Some(t + QUIET));
        assert_eq!(g.due(t + QUIET / 2), None);
        g.note(0, 7, t + QUIET / 2);
        assert!(g.due(t + QUIET).is_none(), "heard again: quiet anew");
        assert_eq!(g.due(t + QUIET / 2 + QUIET), Some((0, vec![(7, None)])));
        // Heard without pause: done at most a second after the first.
        let mut busy = Gather::default();
        let mut at = t;
        while at < t + MOST {
            busy.note(1, 3, at);
            assert_eq!(busy.due(at), None);
            at += QUIET / 2;
        }
        assert_eq!(busy.due(t + MOST), Some((1, vec![(3, None)])));
        assert_eq!(busy.deadline(), None, "nothing left due: idle");
    }

    /// A folder heard by its entries' names keeps them, to stat those
    /// alone; heard of itself, or by too many names, it is read whole.
    #[test]
    fn a_gathering_keeps_the_names_heard() {
        let t = Instant::now();
        let name = |s: &str| Box::<[u8]>::from(s.as_bytes());
        let mut g = Gather::default();
        g.note_entry(0, 1, name("a"), t);
        g.note_entry(0, 1, name("b"), t);
        g.note_entry(0, 2, name("c"), t);
        g.note(0, 2, t);
        let names: HashSet<Box<[u8]>> = [name("a"), name("b")].into();
        assert_eq!(
            g.due(t + MOST),
            Some((0, vec![(1, Some(names)), (2, None)])),
            "one batch"
        );
        for i in 0..=NAMES {
            g.note_entry(0, 3, name(&format!("n{i}")), t);
        }
        assert_eq!(g.take(0), vec![(3, None)], "past the names kept, whole");
    }

    /// After an application, each folder read waits, before it is applied
    /// again, for its share of what the application took, by its entries,
    /// `DUTY` - 1 times over: a large folder heard without pause costs a
    /// share of a core, and a small one read with it does not wait for it.
    /// A sync takes what is gathered at once, all the same.
    #[test]
    fn applying_keeps_to_its_share_of_a_core() {
        let t = Instant::now();
        let mut g = Gather::default();
        let took = Duration::from_millis(50);
        g.applied(0, &[(1, 62_000), (2, 10)], took, t);
        g.note(0, 1, t);
        g.note(0, 2, t);
        assert_eq!(
            g.due(t + MOST),
            Some((0, vec![(2, None)])),
            "the small one waits for nothing"
        );
        let held = g.deadline().unwrap();
        assert!(
            held > t + took * (DUTY - 2),
            "the large one held for its share"
        );
        assert_eq!(
            g.due(held.checked_sub(Duration::from_millis(1)).unwrap()),
            None
        );
        assert_eq!(g.due(held), Some((0, vec![(1, None)])));
        g.applied(0, &[(1, 62_000)], took, t);
        g.note(0, 1, t);
        assert_eq!(g.take(0), vec![(1, None)], "a sync is not held");
    }

    /// Folders limited by the share divide half of it: each held while the
    /// others are is held as many times longer, so that together they take
    /// one part in `2 * DUTY`, all charged to the account. A
    /// small folder is never limited, so never waits on a large one.
    #[test]
    fn folders_the_share_limits_divide_it() {
        let t = Instant::now();
        let took = Duration::from_millis(50);
        let mut g = Gather::default();
        g.applied(0, &[(1, 62_000)], took, t);
        g.applied(0, &[(2, 62_000)], took, t);
        g.applied(0, &[(3, 10)], Duration::from_micros(100), t);
        g.note(0, 1, t);
        g.note(0, 2, t);
        g.note(0, 3, t);
        let until = |g: &Gather, f| g.holds[&(0, f)].until;
        assert_eq!(until(&g, 1), t + took * (2 * DUTY - 1), "alone");
        assert_eq!(until(&g, 2), t + took * (4 * DUTY - 1), "while 1 is held");
        assert!(until(&g, 3) < t + MOST, "a small one is not limited");
        let floor = t.checked_sub(window()).unwrap();
        let charged = at_share(took) + at_share(took) + at_share(Duration::from_micros(100));
        assert_eq!(g.account, Some(floor + charged), "all charged");
        assert_eq!(g.due(t + MOST), Some((0, vec![(3, None)])));
    }

    /// What finding a folder took is charged to its next application, and
    /// a save waits for its share of what the last took.
    #[test]
    fn finding_and_saving_keep_to_the_share() {
        let t = Instant::now();
        let mut g = Gather::default();
        g.note(0, 1, t);
        g.found(0, 1, Duration::from_millis(5));
        assert_eq!(g.due(t + MOST), Some((0, vec![(1, None)])));
        g.applied(0, &[(1, 10)], Duration::from_millis(1), t + MOST);
        let held = g.holds[&(0, 1)].until;
        assert_eq!(held, t + MOST + Duration::from_millis(6) * (DUTY - 1));
        // Asked for by a sync, what finding took is not owed.
        g.note(0, 2, t);
        g.found(0, 2, Duration::from_millis(20));
        assert_eq!(g.take(0), vec![(2, None)]);
        assert!(g.owed.is_empty());

        let mut s = Gather::default();
        s.changed(t);
        assert!(s.save_due(t + SAVE));
        s.saved(Duration::from_millis(500), t + SAVE);
        s.changed(t + SAVE);
        assert!(!s.save_due(t + SAVE * 2), "the last save's share first");
        let after = t + SAVE + Duration::from_millis(500) * (DUTY - 1);
        assert_eq!(s.deadline(), Some(after));
        assert!(s.save_due(after));
    }

    /// All the rest the writer does for what is heard is charged to an
    /// account: past what it lets through at once, nothing due is applied
    /// until it is paid, then the longest due first, one at a time, each
    /// charged before the next. A sync is never held, no burst is owed for
    /// longer than the window, and, owed for good, one folder due a window
    /// is taken a window.
    #[test]
    fn the_rest_keeps_to_its_account() {
        let t = Instant::now();
        let mut g = Gather::default();
        g.spent(BURST / 2, t);
        assert!(g.open(t), "a quiet time lets a burst through");
        g.spent(BURST, t);
        assert!(!g.open(t), "past the burst");
        let paid = t + at_share(BURST / 2);
        assert_eq!(g.account, Some(paid), "what is owed, paid at its share");
        for f in 0..100 {
            g.note(0, f, t);
        }
        assert!(g.due(t + MOST).is_none(), "held by the account");
        assert_eq!(g.deadline(), Some(paid));
        assert_eq!(g.take(0).len(), 100, "a sync is not held");
        for f in 0..100 {
            g.note(0, f, t + Duration::from_millis(u64::from(f)));
        }
        assert_eq!(
            g.due(paid),
            Some((0, vec![(0, None)])),
            "the longest due first"
        );
        g.applied(0, &[(0, 3_000)], Duration::from_millis(1), paid);
        let next = paid + at_share(Duration::from_millis(1));
        assert!(g.due(paid).is_none(), "charged before the next is taken");
        assert_eq!(g.deadline(), Some(next));
        assert_eq!(g.due(next), Some((0, vec![(1, None)])));
        g.spent(Duration::from_secs(60), next);
        assert_eq!(g.account, Some(next + window()), "owed a window at most");
        // Owed for good, one folder due a window is taken a window.
        let mut overdrawn = Gather::default();
        overdrawn.note(0, 7, t);
        overdrawn.note(0, 8, t);
        let ready = t + QUIET;
        overdrawn.spent(Duration::from_secs(60), t);
        overdrawn.spent(Duration::from_secs(60), ready + window() / 2);
        assert!(overdrawn.due(ready + window() / 2).is_none());
        assert_eq!(overdrawn.deadline(), Some(ready + window()));
        let forced = ready + window();
        assert_eq!(overdrawn.due(forced), Some((0, vec![(7, None)])));
        assert!(overdrawn.due(forced).is_none(), "one a window");
        overdrawn.spent(Duration::from_secs(60), forced);
        assert_eq!(overdrawn.deadline(), Some(forced + window()));
        assert_eq!(overdrawn.due(forced + window()), Some((0, vec![(8, None)])));
        // A batch takes, of one root, what fits by what each cost last.
        let mut fitting = Gather::default();
        for f in 0..10 {
            fitting.costs.insert((0, f), BURST / 5);
            fitting.note(0, f, t);
        }
        fitting.note(1, 0, t);
        let batch = fitting.due(t + MOST).unwrap();
        assert_eq!((batch.0, batch.1.len()), (0, 5), "what fits a full account");
        // The least used lately goes before a busy one due earlier.
        let mut ordered = Gather::default();
        ordered.applied(0, &[(1, 10)], Duration::from_micros(10), t);
        ordered.note(0, 1, t);
        ordered.note(0, 2, t + QUIET);
        assert_eq!(
            ordered.due(t + MOST),
            Some((0, vec![(2, None), (1, None)])),
            "the quiet one first"
        );
    }

    /// A reader waiting to batch what it heard is woken as a cookie is made,
    /// the wake given before it waits or after; and none waits while a
    /// cookie is awaited.
    #[test]
    fn a_reader_is_woken_for_a_cookie() {
        let readers = Arc::new(Readers::default());
        let reader = Arc::clone(&readers);
        let (joined, ready) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn(move || {
            reader.join();
            joined.send(()).unwrap();
            let start = Instant::now();
            std::thread::park_timeout(Duration::from_secs(30));
            start.elapsed()
        });
        ready.recv().unwrap();
        readers.wake();
        assert!(waiting.join().unwrap() < Duration::from_secs(5), "woken");
        assert!(readers.awaited());
        readers.set_awaited(false);
        assert!(!readers.awaited());
    }

    /// The check after an overflow waits for quiet, and longer for each
    /// overflow that follows soon.
    #[test]
    fn the_check_after_an_overflow_backs_off() {
        let t = Instant::now();
        let mut g = Gather::default();
        g.overflowed(t);
        assert!(!g.recheck_due(t + BACKOFF / 2));
        g.note(0, 1, t + BACKOFF / 2);
        assert!(!g.recheck_due(t + BACKOFF), "an event pushed it on");
        assert!(g.recheck_due(t + BACKOFF / 2 + BACKOFF));
        assert!(!g.recheck_due(t + BACKOFF * 10), "once said, not again");
        let again = t + BACKOFF * 2;
        g.overflowed(again);
        assert!(!g.recheck_due(again + BACKOFF));
        assert!(g.recheck_due(again + BACKOFF * 2), "doubled");
        let later = again + CALM * 2;
        g.overflowed(later);
        assert!(g.recheck_due(later + BACKOFF), "calm since: afresh");
    }

    /// What the deadline would be, every folder waiting gone over: as it
    /// was found at each turn before the bounds.
    fn exact_deadline(g: &Gather) -> Option<Instant> {
        let gathering = g
            .heard
            .iter()
            .map(|(key, x)| g.ready(key, x))
            .min()
            .map(|at| match g.account {
                None => at,
                Some(open) => {
                    let forced = g
                        .forced
                        .map_or(at + window(), |last| (last + window()).max(at + window()));
                    at.max(open).min(forced)
                }
            });
        [gathering, g.recheck, g.save_at()]
            .into_iter()
            .flatten()
            .min()
    }

    /// The deadline from the bounds is never later than the one found by
    /// going over every folder waiting, so that nothing due waits past
    /// it; and once what is due is taken, it is that one.
    #[test]
    fn the_bounds_are_never_later_than_what_is_due() {
        let mut g = Gather::default();
        let start = Instant::now();
        let mut now = start;
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |m: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % m
        };
        let later = |a: Option<Instant>, b: Option<Instant>| match (a, b) {
            (Some(a), Some(b)) => a > b,
            (Some(_), None) => false,
            (None, b) => b.is_some(),
        };
        for step in 0..20_000 {
            now += Duration::from_millis(next(40));
            let folder = u32::try_from(next(60)).unwrap();
            match next(10) {
                0..=5 => g.note_entry(
                    usize::from(folder % 2 == 0),
                    folder,
                    Box::from(&b"x"[..]),
                    now,
                ),
                6 => g.spent(Duration::from_millis(next(30)), now),
                7 => {
                    // Woken at its deadline, as the writer is: what is due
                    // is taken, or a later deadline found, never one passed
                    // already, which would wake it again at once for nothing.
                    if let Some(at) = g.gathering() {
                        now = now.max(at);
                    }
                    let due = g.due(now);
                    if due.is_none() {
                        assert!(
                            g.gathering().is_none_or(|at| at > now),
                            "step {step}: woken for nothing"
                        );
                    }
                    if let Some((root, folders)) = due {
                        let read: Vec<(FolderId, usize)> =
                            folders.iter().map(|(f, _)| (*f, 100)).collect();
                        // Up to 60 ms: held for seconds, past a new one's quiet.
                        g.applied(root, &read, Duration::from_millis(next(60)), now);
                    }
                }
                8 => g.note(0, folder, now),
                _ => {
                    if next(20) == 0 {
                        let _ = g.take(usize::from(folder % 2 == 0));
                    }
                }
            }
            assert!(
                !later(g.deadline(), exact_deadline(&g)),
                "step {step}: {:?} later than {:?}",
                g.deadline(),
                exact_deadline(&g)
            );
        }
    }
}

#[cfg(test)]
mod turn_cost {
    use super::*;

    /// What the writer's bookkeeping costs at each turn (an event noted,
    /// what is due, the next deadline), with `n` folders waiting and the
    /// account owed. A trial; run by hand.
    #[test]
    #[ignore = "a trial"]
    fn a_turn_costs() {
        for n in [10u32, 150, 500] {
            let mut g = Gather::default();
            let start = Instant::now();
            for f in 0..n {
                g.note_entry(0, f, Box::from(&b"x"[..]), start);
            }
            // Some applied before: held, and used.
            for f in 0..n / 2 {
                g.applied(0, &[(f + n, 10)], Duration::from_millis(2), start);
            }
            g.spent(Duration::from_secs(5), start);
            let now = start + Duration::from_secs(3);
            let rounds = 20_000;
            let t = Instant::now();
            for i in 0..rounds {
                g.note_entry(0, i % n, Box::from(&b"y"[..]), now);
                assert!(g.due(now).is_none());
                std::hint::black_box(g.deadline());
            }
            println!(
                "TURN: {n} folders waiting, the account owed: {:?} a turn",
                t.elapsed() / rounds
            );
        }
    }
}

/// The system's mounts followed for the writer (macOS): each volume
/// mounted or unmounted said, so that the removable volumes a user's
/// engine keeps are followed (`crate::volumes`), and said again a second
/// after, `DiskArbitration` describing a volume a while after it is
/// mounted. Idle, it waits.
#[cfg(target_os = "macos")]
pub fn follow_mounts(commands: &Sender<Command>) {
    let mounts = match skwird_crawl::MountsHeard::new() {
        Ok(m) => m,
        Err(e) => {
            log(&format!(
                "the mounts cannot be followed, so a kept volume is a root from the next start: {e}"
            ));
            return;
        }
    };
    while mounts.wait().is_ok() {
        if commands.send(Command::Mounts).is_err() {
            return;
        }
        let again = commands.clone();
        let _ = std::thread::Builder::new()
            .name("skwird-mounts".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let _ = again.send(Command::Mounts);
            });
    }
}
