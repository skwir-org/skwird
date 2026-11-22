//! Live folders in the engine (`docs/PLAN.md`, phase 2, step 3): what the
//! watcher hears is sent to the writer, which gathers it a folder at a
//! time and reads each folder again once its gathering is done (DESIGN
//! 8). The writer waits with a deadline only while something is due: a
//! gathering, the check after an overflow, a save. Otherwise it waits
//! for a command alone: idle is idle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use skwird_crawl::Watcher;
use skwird_crawl::fanotify::{self, Group};
use skwird_index::block::FolderId;

use crate::engine::{Command, log};

/// A folder's events are gathered until this passes without one...
const QUIET: Duration = Duration::from_millis(100);
/// ...or this long after the first.
const MOST: Duration = Duration::from_secs(1);
/// What was heard is saved this long after the first change unsaved: the
/// log keeps it durable meanwhile.
const SAVE: Duration = Duration::from_secs(10);
/// After an overflow, the check waits for this long without an event,
/// doubling, up to the next, for each overflow that follows soon.
const BACKOFF: Duration = Duration::from_secs(2);
const BACKOFF_MOST: Duration = Duration::from_secs(60);
/// An overflow this long after the last check starts the wait afresh.
const CALM: Duration = Duration::from_secs(300);
/// What the writer does for what is heard (finding each folder, applying,
/// saving) takes one part in this many of the time, the folders it holds
/// dividing that share: large and busy, they cost a share of a core, never
/// the core. Half the budget (DESIGN 10): reading the events, which cannot
/// wait without being lost, keeps the rest.
const DUTY: u32 = 100;

/// What the writer listens to: a user's engine's watcher (inotify), or
/// the system service's group (fanotify).
pub trait Source {
    type Heard;
    fn fd(&self) -> rustix::fd::BorrowedFd<'_>;
    fn generation(&self) -> u64;
    fn read(&self, heard: &mut Vec<Self::Heard>) -> std::io::Result<()>;
    /// What a failed read is said as: nothing heard since proves anything.
    fn overflow() -> Self::Heard;
    fn command(generation: u64, heard: Vec<Self::Heard>) -> Command;
}

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

/// Listen to `source` for the writer: block until it can be read, send
/// what it heard, and again. A read that fails is said as an overflow:
/// nothing heard since proves anything. The mount table is followed too,
/// as skwir follows it: `/proc/self/mountinfo` signals a change with
/// `POLLPRI`, and is read through so that it signals the next.
pub fn listen<S: Source>(source: &S, commands: &Sender<Command>) {
    use std::io::{Read, Seek};
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
/// share holds it longer than a gathering's most: such folders divide the
/// share between them.
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
}

impl Gather {
    /// A change heard in `folder` of root `root`, of the folder itself:
    /// it is to be read whole.
    pub fn note(&mut self, root: usize, folder: FolderId, now: Instant) {
        self.gathered(root, folder, now).names = None;
    }

    /// A change heard in the entry `name` of `folder`: of what the folder
    /// held, that entry alone is to be stat-ed again.
    pub fn note_entry(&mut self, root: usize, folder: FolderId, name: Box<[u8]>, now: Instant) {
        let fresh = !self.heard.contains_key(&(root, folder));
        let g = self.gathered(root, folder, now);
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

    fn gathered(&mut self, root: usize, folder: FolderId, now: Instant) -> &mut Gathered {
        // The check after an overflow waits for the queue to be quiet.
        if let Some(at) = self.recheck.as_mut() {
            *at = (*at).max(now + self.backoff);
        }
        let g = self.heard.entry((root, folder)).or_insert(Gathered {
            first: now,
            last: now,
            names: None,
            owed: Duration::ZERO,
        });
        g.last = now;
        g
    }

    /// Finding `folder` of root `root`, heard, took `took` of the writer's
    /// processor time: charged to its next application.
    pub fn found(&mut self, root: usize, folder: FolderId, took: Duration) {
        if let Some(g) = self.heard.get_mut(&(root, folder)) {
            g.owed += took;
        }
    }

    /// The folders whose gathering is done, and whose hold is past, by
    /// root, let go of here.
    pub fn due(&mut self, now: Instant) -> Vec<(usize, Vec<HeardFolder>)> {
        self.holds.retain(|_, hold| now < hold.until);
        let mut by_root: HashMap<usize, Vec<HeardFolder>> = HashMap::new();
        let done: Vec<(usize, FolderId)> = self
            .heard
            .iter()
            .filter(|(key, g)| {
                (now >= g.last + QUIET || now >= g.first + MOST) && !self.holds.contains_key(key)
            })
            .map(|(&key, _)| key)
            .collect();
        for (root, folder) in done {
            if let Some(g) = self.heard.remove(&(root, folder)) {
                if !g.owed.is_zero() {
                    self.owed.insert((root, folder), g.owed);
                }
                by_root.entry(root).or_default().push((folder, g.names));
            }
        }
        let mut due: Vec<_> = by_root.into_iter().collect();
        for (_, folders) in &mut due {
            folders.sort_unstable_by_key(|(folder, _)| *folder);
        }
        due.sort_unstable_by_key(|(root, _)| *root);
        due
    }

    /// The folders of `root` read, each with the entries its listing held,
    /// took `took` of processor time. Each is held, before it is applied
    /// again, for its share of that time (by its entries) and of what
    /// finding it took, [`DUTY`] - 1 times over: a large folder heard
    /// without pause waits for itself, and a small one read with it does
    /// not. A folder held longer than a gathering's most is limited by the
    /// share; such folders divide it, each held as many times longer as
    /// there are others: together they take one part in [`DUTY`], however
    /// many are busy. Their answers lag then, live still; a sync applies
    /// them at once.
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
            return;
        }
        self.holds.retain(|_, hold| now < hold.until);
        let limited = self.holds.values().filter(|hold| hold.limited).count();
        let sharing = u32::try_from(limited + 1).unwrap_or(u32::MAX);
        for (&(folder, n), owed) in read.iter().zip(owed) {
            let share = took.mul_f64((n + 1) as f64 / total as f64) + owed;
            let alone = share * (DUTY - 1);
            let wait = if alone > MOST {
                share * DUTY.saturating_mul(sharing).saturating_sub(1)
            } else {
                alone
            };
            if wait >= Duration::from_millis(1) {
                self.holds.insert(
                    (root, folder),
                    Hold {
                        until: now + wait,
                        limited: alone > MOST,
                    },
                );
            }
        }
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
    /// share of it, as a folder's application does.
    pub fn saved(&mut self, took: Duration, now: Instant) {
        self.save_held = Some(now + took * (DUTY - 1));
    }

    fn save_at(&self) -> Option<Instant> {
        let at = self.unsaved? + SAVE;
        Some(self.save_held.map_or(at, |held| at.max(held)))
    }

    /// When something is due next, if anything is.
    pub fn deadline(&self) -> Option<Instant> {
        let gathering = self
            .heard
            .iter()
            .map(|(key, g)| {
                let at = (g.last + QUIET).min(g.first + MOST);
                // Held after its last application, a folder waits for it.
                self.holds.get(key).map_or(at, |hold| at.max(hold.until))
            })
            .min();
        [gathering, self.recheck, self.save_at()]
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
        assert_eq!(g.due(t + QUIET / 2), Vec::new());
        g.note(0, 7, t + QUIET / 2);
        assert!(g.due(t + QUIET).is_empty(), "heard again: quiet anew");
        assert_eq!(g.due(t + QUIET / 2 + QUIET), vec![(0, vec![(7, None)])]);
        // Heard without pause: done at most a second after the first.
        let mut busy = Gather::default();
        let mut at = t;
        while at < t + MOST {
            busy.note(1, 3, at);
            assert_eq!(busy.due(at), Vec::new());
            at += QUIET / 2;
        }
        assert_eq!(busy.due(t + MOST), vec![(1, vec![(3, None)])]);
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
        let due = g.due(t + MOST);
        let names: HashSet<Box<[u8]>> = [name("a"), name("b")].into();
        assert_eq!(due, vec![(0, vec![(1, Some(names)), (2, None)])]);
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
            vec![(0, vec![(2, None)])],
            "the small one waits for nothing"
        );
        let held = g.deadline().unwrap();
        assert!(
            held > t + took * (DUTY - 2),
            "the large one held for its share"
        );
        assert_eq!(
            g.due(held.checked_sub(Duration::from_millis(1)).unwrap()),
            Vec::new()
        );
        assert_eq!(g.due(held), vec![(0, vec![(1, None)])]);
        g.applied(0, &[(1, 62_000)], took, t);
        g.note(0, 1, t);
        assert_eq!(g.take(0), vec![(1, None)], "a sync is not held");
    }

    /// Folders limited by the share divide it: each held while the others
    /// are is held as many times longer, so that together they take one
    /// part in `DUTY`. A small folder is never limited, so never waits on
    /// a large one.
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
        assert_eq!(until(&g, 1), t + took * (DUTY - 1), "alone");
        assert_eq!(until(&g, 2), t + took * (2 * DUTY - 1), "while 1 is held");
        assert!(until(&g, 3) < t + MOST, "a small one is not limited");
        assert_eq!(g.due(t + MOST), vec![(0, vec![(3, None)])]);
    }

    /// What finding a folder took is charged to its next application, and
    /// a save waits for its share of what the last took.
    #[test]
    fn finding_and_saving_keep_to_the_share() {
        let t = Instant::now();
        let mut g = Gather::default();
        g.note(0, 1, t);
        g.found(0, 1, Duration::from_millis(20));
        assert_eq!(g.due(t + MOST), vec![(0, vec![(1, None)])]);
        g.applied(0, &[(1, 10)], Duration::from_millis(1), t + MOST);
        let held = g.holds[&(0, 1)].until;
        assert_eq!(held, t + MOST + Duration::from_millis(21) * (DUTY - 1));
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
}
