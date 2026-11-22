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
/// Applying what is heard takes at most one part in this many of the
/// time: a folder that is large and busy costs a share of a core, never
/// the core.
const DUTY: u32 = 50;

/// Listen to `watcher` for the writer: block until it can be read, send
/// what it heard, and again. A read that fails is said as an overflow:
/// nothing heard since proves anything. The mount table is followed too,
/// as skwir follows it: `/proc/self/mountinfo` signals a change with
/// `POLLPRI`, and is read through so that it signals the next.
pub fn listen(watcher: &Watcher, commands: &Sender<Command>) {
    use std::io::{Read, Seek};
    let mut heard = Vec::new();
    let mut table = std::fs::File::open("/proc/self/mountinfo").ok();
    let mut skip = Vec::new();
    if let Some(t) = table.as_mut() {
        let _ = t.read_to_end(&mut skip);
    }
    loop {
        let fd = watcher.fd();
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
        if let Err(e) = watcher.read(&mut heard) {
            log(&format!("cannot read what the watcher heard: {e}"));
            heard.push(skwird_crawl::Heard::Overflow);
        }
        if heard.is_empty() {
            continue;
        }
        let command = Command::Heard {
            generation: watcher.generation(),
            heard: std::mem::take(&mut heard),
        };
        if commands.send(command).is_err() {
            return;
        }
    }
}

pub use skwird_index::update::HeardFolder;

/// Past this many names, a folder is read whole: what is kept of a
/// gathering stays bounded.
const NAMES: usize = 4096;

/// A folder's gathering: when first heard, when last, and the names heard.
struct Gathered {
    first: Instant,
    last: Instant,
    names: Option<HashSet<Box<[u8]>>>,
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
    /// Until when each folder applied is not applied again: what its
    /// reads took, fifty times over.
    holds: HashMap<(usize, FolderId), Instant>,
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
        });
        g.last = now;
        g
    }

    /// The folders whose gathering is done, and whose hold is past, by
    /// root, let go of here.
    pub fn due(&mut self, now: Instant) -> Vec<(usize, Vec<HeardFolder>)> {
        self.holds.retain(|_, until| now < *until);
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
    /// again, for its share of that time (by its entries) fifty times
    /// over: applying takes at most one part in [`DUTY`] of the time, and
    /// a large folder heard without pause waits for itself alone. Its
    /// answers lag then, live still; a sync applies it at once.
    pub fn applied(
        &mut self,
        root: usize,
        read: &[(FolderId, usize)],
        took: Duration,
        now: Instant,
    ) {
        let total: usize = read.iter().map(|(_, n)| n + 1).sum();
        if total == 0 {
            return;
        }
        for &(folder, n) in read {
            let share = took.mul_f64((n + 1) as f64 / total as f64) * (DUTY - 1);
            if share >= Duration::from_millis(1) {
                self.holds.insert((root, folder), now + share);
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

    /// Whether the indexes changed by what was heard are to be saved now.
    pub fn save_due(&mut self, now: Instant) -> bool {
        if self.unsaved.is_some_and(|at| now >= at + SAVE) {
            self.unsaved = None;
            return true;
        }
        false
    }

    /// When something is due next, if anything is.
    pub fn deadline(&self) -> Option<Instant> {
        let gathering = self
            .heard
            .iter()
            .map(|(key, g)| {
                let at = (g.last + QUIET).min(g.first + MOST);
                // Held after its last application, a folder waits for it.
                self.holds.get(key).map_or(at, |&until| at.max(until))
            })
            .min();
        [gathering, self.recheck, self.unsaved.map(|at| at + SAVE)]
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
