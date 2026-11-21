//! The watcher (`docs/PLAN.md`, phase 2, step 1): an inotify instance,
//! read through rustix, watching the folders a crawl or a reread reads
//! within the subtrees it is given.
//!
//! - **The folder watched is the folder read.** A folder is watched
//!   through its open handle (`/proc/self/fd/N`), never by its path: a
//!   folder put in its place between the open and the watch cannot be
//!   watched for it. The watch is made before its entries are read, so a
//!   change between the two is heard.
//! - **Events name folders,** by the watch descriptor of the folder they
//!   happened in. Whoever applies them maps descriptors to folders, and
//!   reads each folder named again: the watcher keeps no state of what a
//!   folder holds, so it cannot disagree with a read.
//! - **An overflow is said,** as [`Heard::Overflow`]: events were dropped,
//!   and nothing heard since the last proof proves anything.
//! - **Each instance has its generation** ([`Watcher::generation`]): one
//!   made again leaves the old one's events applied, but proving nothing
//!   (DESIGN 2).
//! - **Within a budget** of watches: past it, a folder is not watched, and
//!   says so ([`Watch::Budget`]), as it says the user's limit reached.

use std::collections::HashSet;
use std::fmt;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

use crate::Watch;

/// What a watcher heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Heard {
    /// An entry of the folder watched by this descriptor was made,
    /// removed, moved, written to or given other attributes, or the
    /// folder's own: read it again.
    Changed(i32),
    /// The folder itself moved: its old parent and its new say where.
    Moved(i32),
    /// The folder's watch is gone: the folder was removed, or its
    /// filesystem unmounted.
    Gone(i32),
    /// Events were dropped: what was heard proves nothing until a check.
    Overflow,
}

/// What a folder is watched for: every change to what it holds, and to
/// itself.
fn mask() -> WatchFlags {
    WatchFlags::CREATE
        | WatchFlags::DELETE
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::MODIFY
        | WatchFlags::ATTRIB
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
        | WatchFlags::ONLYDIR
        // An entry removed while open says nothing more.
        | WatchFlags::EXCL_UNLINK
}

struct State {
    /// The folders whose subtrees are watched, as the system resolves
    /// them.
    scopes: Vec<PathBuf>,
    budget: usize,
    /// The descriptors watching now.
    watching: HashSet<i32>,
}

/// An inotify instance and what it watches.
pub struct Watcher {
    fd: OwnedFd,
    generation: u64,
    state: Mutex<State>,
    buffer: Mutex<Box<[MaybeUninit<u8>]>>,
}

impl fmt::Debug for Watcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher")
            .field("generation", &self.generation)
            .field("watching", &self.watching())
            .finish_non_exhaustive()
    }
}

impl Watcher {
    /// An instance of generation `generation`, watching at most `budget`
    /// folders, and none until it is given subtrees to watch.
    pub fn new(generation: u64, budget: usize) -> std::io::Result<Watcher> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        Ok(Watcher {
            fd,
            generation,
            state: Mutex::new(State {
                scopes: Vec::new(),
                budget,
                watching: HashSet::new(),
            }),
            buffer: Mutex::new(vec![MaybeUninit::uninit(); 64 << 10].into_boxed_slice()),
        })
    }

    /// The share of the user's watches a user's engine takes: a quarter
    /// of the system's limit for each user, since editors, file managers
    /// and build tools share the rest (open question 2).
    pub fn share() -> usize {
        std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map_or(8192, |limit| limit / 4)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Its handle, for a caller to wait on until it can be read.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// How many folders it watches.
    pub fn watching(&self) -> usize {
        self.lock().watching.len()
    }

    /// Watch, from now on, the folders read within these subtrees, given
    /// as the system resolves them. Watches made stay until taken back.
    pub fn set_scopes(&self, scopes: Vec<PathBuf>) {
        self.lock().scopes = scopes;
    }

    /// Whether a folder at `path` (as the system resolves it) is within a
    /// subtree to watch.
    pub fn wants(&self, path: &Path) -> bool {
        self.lock().scopes.iter().any(|s| path.starts_with(s))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Watch the folder open as `dir`, at `path`, if it is within a
    /// subtree to watch: through its handle, before its entries are read.
    pub(crate) fn watch(&self, dir: BorrowedFd<'_>, path: &Path) -> Watch {
        let mut state = self.lock();
        if !state.scopes.iter().any(|s| path.starts_with(s)) {
            return Watch::Unasked;
        }
        if state.watching.len() >= state.budget {
            return Watch::Budget;
        }
        let handle = format!("/proc/self/fd/{}", rustix::fd::AsRawFd::as_raw_fd(&dir));
        match inotify::add_watch(&self.fd, handle.as_str(), mask()) {
            Ok(wd) => {
                state.watching.insert(wd);
                Watch::Watched(wd)
            }
            Err(Errno::NOSPC) => Watch::Limit,
            Err(e) => Watch::Failed(e.raw_os_error()),
        }
    }

    /// Stop watching by this descriptor.
    pub fn unwatch(&self, wd: i32) {
        if self.lock().watching.remove(&wd) {
            // Gone already if its folder was removed: the kernel says so in
            // turn, and nothing is lost by its error.
            let _ = inotify::remove_watch(&self.fd, wd);
        }
    }

    /// Everything heard and not read yet, added to `heard`, without
    /// waiting: [`Watcher::fd`] says when there is more.
    pub fn read(&self, heard: &mut Vec<Heard>) -> std::io::Result<()> {
        let mut buffer = self
            .buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        loop {
            let event = match reader.next() {
                Ok(event) => event,
                Err(Errno::AGAIN) => return Ok(()),
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            };
            let (wd, flags) = (event.wd(), event.events());
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                heard.push(Heard::Overflow);
            } else if flags.contains(ReadFlags::IGNORED) {
                self.lock().watching.remove(&wd);
                heard.push(Heard::Gone(wd));
            } else if flags.contains(ReadFlags::UNMOUNT) {
                heard.push(Heard::Gone(wd));
            } else if flags.contains(ReadFlags::MOVE_SELF) && event.file_name().is_none() {
                heard.push(Heard::Moved(wd));
            } else if flags.contains(ReadFlags::DELETE_SELF) {
                // Its watch's end follows, as `IGNORED`.
            } else {
                heard.push(Heard::Changed(wd));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use skwird_testkit::Scratch;

    use super::*;
    use crate::{Cancel, Crawl, Exclusions, Options, Outcome, Root, crawl_all};

    fn opts(watcher: &Arc<Watcher>) -> Options {
        Options {
            watcher: Some(watcher.clone()),
            threads: 3,
            ..Options::default()
        }
    }

    /// A crawl of the scratch tree, `scopes` (below it) watched.
    fn crawled(s: &Scratch, watcher: &Arc<Watcher>, scopes: &[&str], excluded: &[&str]) -> Crawl {
        let (ex, _) = Exclusions::resolve(excluded.iter().map(|e| s.join(e)));
        let root = Root::open(s.path(), ex).unwrap();
        watcher.set_scopes(scopes.iter().map(|p| root.path().join(p)).collect());
        crawl_all(&root, &opts(watcher), &Cancel::default()).unwrap()
    }

    /// Each folder's watch, by its path below the root.
    fn watches(crawl: &Crawl) -> BTreeMap<PathBuf, Watch> {
        crawl
            .folders
            .iter()
            .map(|f| (f.rel.clone(), f.watch))
            .collect()
    }

    fn wd(crawl: &Crawl, rel: &str) -> i32 {
        match watches(crawl)[Path::new(rel)] {
            Watch::Watched(wd) => wd,
            other => panic!("{rel}: {other:?}"),
        }
    }

    fn heard(w: &Watcher) -> Vec<Heard> {
        let mut heard = Vec::new();
        w.read(&mut heard).unwrap();
        heard
    }

    /// A change in a watched folder is heard, named by its folder alone; a
    /// folder removed says its watch is gone.
    #[test]
    fn a_change_is_heard_named_by_its_folder() {
        let s = Scratch::new("watch-heard");
        s.file("a/one");
        s.file("b/two");
        s.dir("b/c");
        let w = Arc::new(Watcher::new(1, 100).unwrap());
        let crawl = crawled(&s, &w, &[""], &[]);
        let (wa, wb, wc) = (wd(&crawl, "a"), wd(&crawl, "b"), wd(&crawl, "b/c"));
        assert_eq!(
            BTreeSet::from([wd(&crawl, ""), wa, wb, wc]).len(),
            4,
            "one descriptor a folder"
        );
        assert_eq!(w.watching(), 4);
        assert_eq!(heard(&w), [], "nothing heard of the crawl's own reads");
        s.file("a/new");
        assert_eq!(heard(&w), [Heard::Changed(wa)]);
        s.remove("b/c");
        let got = heard(&w);
        assert!(got.contains(&Heard::Changed(wb)), "{got:?}");
        assert!(got.contains(&Heard::Gone(wc)), "{got:?}");
        assert!(!got.contains(&Heard::Changed(wa)), "{got:?}");
        assert_eq!(w.watching(), 3, "its watch let go of");
        s.rename("a/one", "b/one");
        let got = heard(&w);
        assert!(got.contains(&Heard::Changed(wa)) && got.contains(&Heard::Changed(wb)));
    }

    /// Files made in every folder while the crawl reads and watches them:
    /// each folder whose files differ from its listing was heard changed,
    /// since it was watched before it was read.
    #[test]
    fn a_change_made_while_the_watches_are_installed_is_not_missed() {
        let s = Scratch::new("watch-install");
        let folders: Vec<String> = (0..300).map(|i| format!("f{i:03}")).collect();
        for f in &folders {
            s.file(format!("{f}/first"));
        }
        let w = Arc::new(Watcher::new(1, 1000).unwrap());
        let dirs: Vec<PathBuf> = folders.iter().map(|f| s.join(f)).collect();
        let stop = AtomicBool::new(false);
        let crawl = std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    for dir in &dirs {
                        let _ = std::fs::write(dir.join(format!("late {n}")), b"");
                    }
                    n += 1;
                }
            });
            let crawl = crawled(&s, &w, &[""], &[]);
            stop.store(true, Ordering::Relaxed);
            crawl
        });
        let got: BTreeSet<Heard> = heard(&w).into_iter().collect();
        let mut differed = 0;
        for folder in crawl
            .folders
            .iter()
            .filter(|f| !f.rel.as_os_str().is_empty())
        {
            let Outcome::Listed(listing) = &folder.outcome else {
                panic!("{:?}", folder.outcome)
            };
            let listed: BTreeSet<Vec<u8>> = listing.iter().map(|(n, _)| n.to_vec()).collect();
            let on_disk: BTreeSet<Vec<u8>> = std::fs::read_dir(s.join(&folder.rel))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_encoded_bytes())
                .collect();
            if listed != on_disk {
                differed += 1;
                let Watch::Watched(wd) = folder.watch else {
                    panic!("{:?}", folder.watch)
                };
                assert!(
                    got.contains(&Heard::Changed(wd)),
                    "{} changed after it was read, unheard",
                    folder.rel.display()
                );
            }
        }
        assert!(
            differed > 0,
            "the writer made nothing after a folder was read"
        );
    }

    /// More events than the queue holds, unread: the overflow is said.
    #[test]
    fn an_overflow_is_said() {
        let s = Scratch::new("watch-overflow");
        s.dir("busy");
        let w = Arc::new(Watcher::new(1, 10).unwrap());
        crawled(&s, &w, &["busy"], &[]);
        let most: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for i in 0..most + 64 {
            std::fs::write(s.join(format!("busy/{i}")), b"").unwrap();
        }
        assert!(heard(&w).contains(&Heard::Overflow));
    }

    /// An instance made again while the old one's events are still read:
    /// each says its own generation, so what the old one hears can be
    /// told from what the new one does.
    #[test]
    fn an_instance_made_again_hears_with_its_own_generation() {
        let s = Scratch::new("watch-generation");
        s.dir("a");
        let old = Arc::new(Watcher::new(1, 10).unwrap());
        crawled(&s, &old, &[""], &[]);
        s.file("a/before");
        let new = Arc::new(Watcher::new(2, 10).unwrap());
        let crawl = crawled(&s, &new, &[""], &[]);
        s.file("a/after");
        assert_eq!((old.generation(), new.generation()), (1, 2));
        assert_eq!(heard(&old).len(), 2, "the old one still hears both");
        assert_eq!(heard(&new), [Heard::Changed(wd(&crawl, "a"))]);
    }

    /// Past its budget, a folder is not watched, and says so.
    #[test]
    fn a_budget_reached_is_said() {
        let s = Scratch::new("watch-budget");
        for f in ["a", "b", "c", "d"] {
            s.dir(f);
        }
        let w = Arc::new(Watcher::new(1, 3).unwrap());
        let crawl = crawled(&s, &w, &[""], &[]);
        let all = watches(&crawl);
        let watched = all
            .values()
            .filter(|w| matches!(w, Watch::Watched(_)))
            .count();
        let over = all.values().filter(|w| **w == Watch::Budget).count();
        assert_eq!((watched, over), (3, 2), "{all:?}");
        assert_eq!(w.watching(), 3);
    }

    /// Only folders within a subtree to watch are watched; an excluded one
    /// is never read, so never watched.
    #[test]
    fn only_the_subtrees_given_are_watched() {
        let s = Scratch::new("watch-scopes");
        s.dir("in/a");
        s.dir("in/secret");
        s.dir("out/b");
        let w = Arc::new(Watcher::new(1, 10).unwrap());
        let crawl = crawled(&s, &w, &["in"], &["in/secret"]);
        let all = watches(&crawl);
        for rel in ["in", "in/a"] {
            assert!(matches!(all[Path::new(rel)], Watch::Watched(_)), "{rel}");
        }
        for rel in ["", "out", "out/b", "in/secret"] {
            assert_eq!(all[Path::new(rel)], Watch::Unasked, "{rel}");
        }
        assert_eq!(w.watching(), 2);
    }
}
