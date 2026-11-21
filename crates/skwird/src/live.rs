//! Live folders in the engine (`docs/PLAN.md`, phase 2, step 3): what the
//! watcher hears is sent to the writer, which gathers it a folder at a
//! time and reads each folder again once its gathering is done (DESIGN
//! 8). The writer waits with a deadline only while something is due: a
//! gathering, the check after an overflow, a save. Otherwise it waits
//! for a command alone: idle is idle.

use std::collections::HashMap;
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

/// What the writer has heard and not applied, and what is due after.
#[derive(Default)]
pub struct Gather {
    /// Each folder heard, by root: when first heard, and last.
    heard: HashMap<(usize, FolderId), (Instant, Instant)>,
    /// When the check after an overflow is due.
    recheck: Option<Instant>,
    backoff: Duration,
    last_check: Option<Instant>,
    /// Since when what was heard has not been saved.
    unsaved: Option<Instant>,
}

impl Gather {
    /// A change heard in `folder` of root `root`.
    pub fn note(&mut self, root: usize, folder: FolderId, now: Instant) {
        self.heard
            .entry((root, folder))
            .and_modify(|(_, last)| *last = now)
            .or_insert((now, now));
        // The check after an overflow waits for the queue to be quiet.
        if let Some(at) = self.recheck.as_mut() {
            *at = (*at).max(now + self.backoff);
        }
    }

    /// The folders whose gathering is done, by root, let go of here.
    pub fn due(&mut self, now: Instant) -> Vec<(usize, Vec<FolderId>)> {
        let mut by_root: HashMap<usize, Vec<FolderId>> = HashMap::new();
        self.heard.retain(|&(root, folder), &mut (first, last)| {
            let done = now >= last + QUIET || now >= first + MOST;
            if done {
                by_root.entry(root).or_default().push(folder);
            }
            !done
        });
        let mut due: Vec<_> = by_root.into_iter().collect();
        for (_, folders) in &mut due {
            folders.sort_unstable();
        }
        due.sort_unstable_by_key(|(root, _)| *root);
        due
    }

    /// Every folder of `root` heard, done gathering or not: a sync takes
    /// them at once.
    pub fn take(&mut self, root: usize) -> Vec<FolderId> {
        let mut folders = Vec::new();
        self.heard.retain(|&(r, folder), _| {
            let take = r == root;
            if take {
                folders.push(folder);
            }
            !take
        });
        folders.sort_unstable();
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
            .values()
            .map(|&(first, last)| (last + QUIET).min(first + MOST))
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
        assert!(g.due(t + QUIET / 2).is_empty());
        g.note(0, 7, t + QUIET / 2);
        assert!(g.due(t + QUIET).is_empty(), "heard again: quiet anew");
        assert_eq!(g.due(t + QUIET / 2 + QUIET), vec![(0, vec![7])]);
        // Heard without pause: done at most a second after the first.
        let mut busy = Gather::default();
        let mut at = t;
        while at < t + MOST {
            busy.note(1, 3, at);
            assert!(busy.due(at).is_empty());
            at += QUIET / 2;
        }
        assert_eq!(busy.due(t + MOST), vec![(1, vec![3])]);
        assert_eq!(busy.deadline(), None, "nothing left due: idle");
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
