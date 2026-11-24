//! What the crawl and the check do on every system: the crawl's folders
//! shared among threads, the folders an index stored, the check's
//! verdicts gathered. What reads a folder, and what stats one, is each
//! system's (`linux`, `windows`), through `sys`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::sys::{self, Root};
use crate::{
    Cancel, Crawled, Entry, Error, FileId, FolderStat, Kind, Options, Outcome, Time, Watch, Why,
};

/// Read the folder at `rel` below the root: its stat first, then its
/// entries, each stat-ed through its handle (all of them with
/// [`Options::attrs`], folders alone without). Gives its stat where it
/// was taken, and what came of it.
pub fn read_folder(root: &Root, rel: &Path, opts: &Options) -> (Option<FolderStat>, Outcome) {
    sys::read(root, rel, opts, &mut Watch::Unasked, None)
}

/// An entry kept from what was read before, by its name and the inode
/// and kind a listing gives it: none to stat it.
pub type Reuse<'a> = &'a dyn Fn(&[u8], u64, Kind) -> Option<Entry>;

/// [`read_folder_watched`], every entry `reuse` gives taken as it is, not
/// stat-ed: a live folder read again stats what an event named, and what
/// is new, alone (DESIGN 8). Its names are still every name its listing
/// holds.
pub fn read_folder_reusing(
    root: &Root,
    rel: &Path,
    opts: &Options,
    reuse: Reuse<'_>,
) -> (Option<FolderStat>, Outcome, Watch) {
    let mut watch = Watch::Unasked;
    let (stat, outcome) = sys::read(root, rel, opts, &mut watch, Some(reuse));
    (stat, outcome, watch)
}

/// [`read_folder`], and what came of watching the folder: with
/// [`Options::watcher`], a folder within its subtrees is watched through
/// its handle before its entries are read.
pub fn read_folder_watched(
    root: &Root,
    rel: &Path,
    opts: &Options,
) -> (Option<FolderStat>, Outcome, Watch) {
    let mut watch = Watch::Unasked;
    let (stat, outcome) = sys::read(root, rel, opts, &mut watch, None);
    (stat, outcome, watch)
}

pub(crate) fn mount_why(network: bool) -> Why {
    if network {
        Why::NetworkMount
    } else {
        Why::Mount
    }
}

/// A folder waiting to be read: its identity where its parent's listing
/// gave it (none for the folder a crawl starts at, unless given).
struct Job {
    slot: usize,
    rel: PathBuf,
    id: Option<FileId>,
    parent: Option<usize>,
}

/// What reads a folder for a crawl: the system's own read
/// ([`read_folder_watched`]) unless another is given, as the system
/// service gives its volume's file table on NTFS (`mft::TableReader`).
/// Given the folder's path below the root, and its identity where its
/// parent's listing gave it.
pub type Reader<'a> = &'a (
        dyn Fn(&Root, &Path, Option<FileId>, &Options) -> (Option<FolderStat>, Outcome, Watch)
            + Sync
    );

/// Folders waiting to be read, and how many are being read: a crawl ends
/// when none waits and none is read.
struct Queue {
    state: Mutex<(Vec<Job>, usize)>,
    turn: Condvar,
}

impl Queue {
    fn next(&self) -> Option<Job> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(job) = state.0.pop() {
                state.1 += 1;
                return Some(job);
            }
            if state.1 == 0 {
                return None;
            }
            state = self.turn.wait(state).unwrap();
        }
    }

    fn done(&self, found: Vec<Job>) {
        let mut state = self.state.lock().unwrap();
        state.1 -= 1;
        state.0.extend(found);
        self.turn.notify_all();
    }
}

/// Read the folder at `from` below the root, and every folder below it,
/// on [`Options::threads`] threads. Each folder reached is handed to
/// `sink` with its slot, once: the folder at `from` has slot 0, and every
/// other names its parent's. A folder not read (excluded, a mount, a
/// failed read) is handed as not covered, with why. Gives how long it
/// took.
pub fn crawl(
    root: &Root,
    from: &Path,
    opts: &Options,
    cancel: &Cancel,
    sink: &(dyn Fn(usize, Crawled) + Sync),
) -> Result<Duration, Error> {
    crawl_by(
        root,
        from,
        None,
        opts,
        cancel,
        sink,
        &|root, rel, _, opts| read_folder_watched(root, rel, opts),
    )
}

/// [`crawl`], each folder read by `reader`, the folder at `from` known as
/// `id` where given.
pub fn crawl_by(
    root: &Root,
    from: &Path,
    id: Option<FileId>,
    opts: &Options,
    cancel: &Cancel,
    sink: &(dyn Fn(usize, Crawled) + Sync),
    reader: Reader<'_>,
) -> Result<Duration, Error> {
    let start = Instant::now();
    let next_slot = AtomicUsize::new(1);
    let stopped = AtomicBool::new(false);
    let queue = Queue {
        state: Mutex::new((
            vec![Job {
                slot: 0,
                rel: from.to_path_buf(),
                id,
                parent: None,
            }],
            0,
        )),
        turn: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..opts.threads.max(1) {
            scope.spawn(|| {
                if opts.background {
                    crate::pace::background();
                }
                while let Some(job) = queue.next() {
                    if cancel.is_cancelled() {
                        stopped.store(true, Ordering::Relaxed);
                        queue.done(Vec::new());
                        continue;
                    }
                    let found = visit(root, job, opts, &next_slot, sink, reader);
                    queue.done(found);
                }
            });
        }
    });
    if stopped.into_inner() {
        return Err(Error::Cancelled);
    }
    Ok(start.elapsed())
}

/// Read one folder, hand it to the sink, and give the folders below it
/// to read next. Those not to be read are handed to the sink at once.
fn visit(
    root: &Root,
    job: Job,
    opts: &Options,
    next_slot: &AtomicUsize,
    sink: &(dyn Fn(usize, Crawled) + Sync),
    reader: Reader<'_>,
) -> Vec<Job> {
    let (stat, outcome, watch) = reader(root, &job.rel, job.id, opts);
    let mut found = Vec::new();
    if let Outcome::Listed(listing) = &outcome {
        for (name, entry) in listing.iter() {
            if entry.kind != Kind::Folder {
                continue;
            }
            let rel = job.rel.join(sys::component(name));
            let slot = next_slot.fetch_add(1, Ordering::Relaxed);
            let not_read = |why| Crawled {
                rel: rel.clone(),
                parent: Some(job.slot),
                stat: None,
                outcome: Outcome::NotCovered(why),
                watch: Watch::Unasked,
            };
            if let Some(network) = root.mount_at(&rel) {
                sink(slot, not_read(mount_why(network)));
            } else if root
                .exclusions()
                .holds(Some(entry.id), &root.path().join(&rel))
            {
                sink(slot, not_read(Why::Excluded));
            } else {
                found.push(Job {
                    slot,
                    rel,
                    id: Some(entry.id),
                    parent: Some(job.slot),
                });
            }
        }
    }
    sink(
        job.slot,
        Crawled {
            rel: job.rel,
            parent: job.parent,
            stat,
            outcome,
            watch,
        },
    );
    found
}

/// Every folder a crawl of the whole root reached, by slot: the root
/// first.
pub struct Crawl {
    pub folders: Vec<Crawled>,
    pub took: Duration,
}

/// Crawl the whole root, and keep every folder reached.
pub fn crawl_all(root: &Root, opts: &Options, cancel: &Cancel) -> Result<Crawl, Error> {
    crawl_below(root, Path::new(""), opts, cancel)
}

/// Crawl the folder at `from` below the root and every folder below it,
/// and keep every folder reached, by slot: `from` first.
pub fn crawl_below(
    root: &Root,
    from: &Path,
    opts: &Options,
    cancel: &Cancel,
) -> Result<Crawl, Error> {
    crawl_below_by(root, from, None, opts, cancel, &|root, rel, _, opts| {
        read_folder_watched(root, rel, opts)
    })
}

/// [`crawl_below`], each folder read by `reader` ([`crawl_by`]), the
/// folder at `from` known as `id` where given.
pub fn crawl_below_by(
    root: &Root,
    from: &Path,
    id: Option<FileId>,
    opts: &Options,
    cancel: &Cancel,
    reader: Reader<'_>,
) -> Result<Crawl, Error> {
    let slots: Mutex<Vec<Option<Crawled>>> = Mutex::new(Vec::new());
    let sink = |slot: usize, crawled| {
        let mut slots = slots.lock().unwrap();
        if slots.len() <= slot {
            slots.resize_with(slot + 1, || None);
        }
        slots[slot] = Some(crawled);
    };
    let took = crawl_by(root, from, id, opts, cancel, &sink, reader)?;
    let folders = slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|c| c.expect("a crawl hands every slot it gives"))
        .collect();
    Ok(Crawl { folders, took })
}

/// A folder as the index stored it, for the check: its path is kept in
/// [`Stored`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoredFolder {
    pub id: FileId,
    pub ctime: Time,
    /// Read it again whatever its times say: it was read within a tick of
    /// its last change, or its read failed.
    pub again: bool,
}

/// The folders an index stored, for the check, numbered as they were
/// given. Their paths below the root are kept end to end in one buffer:
/// a check of a million folders makes no allocation for each, which the
/// allocator would keep once freed (`docs/DESIGN.md`, decision 10).
#[derive(Clone, Debug, Default)]
pub struct Stored {
    paths: Vec<u8>,
    /// Each folder, with where its path ends.
    folders: Vec<(usize, StoredFolder)>,
}

impl Stored {
    /// Room for `folders` folders whose paths take about `bytes` in all,
    /// made at once: one large allocation, which the allocator gives back
    /// whole once freed, where a buffer grown by doubling leaves the
    /// smaller ones behind in the heap.
    pub fn with_capacity(folders: usize, bytes: usize) -> Stored {
        Stored {
            paths: Vec::with_capacity(bytes),
            folders: Vec::with_capacity(folders),
        }
    }

    /// The folder at `rel` below the root.
    pub fn push(&mut self, rel: &Path, folder: StoredFolder) {
        sys::push_path(&mut self.paths, rel);
        self.folders.push((self.paths.len(), folder));
    }

    pub fn len(&self) -> usize {
        self.folders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    /// The `i`th folder's path below the root: borrowed where the system
    /// names paths with bytes, made where it does not (Windows).
    pub fn rel(&self, i: usize) -> std::borrow::Cow<'_, Path> {
        let start = if i == 0 { 0 } else { self.folders[i - 1].0 };
        sys::path_of(&self.paths[start..self.folders[i].0])
    }

    pub fn folder(&self, i: usize) -> StoredFolder {
        self.folders[i].1
    }
}

/// Why the check says a folder is to be read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    /// Its change time is not the one stored: its entries changed.
    Changed,
    /// Its path leads to another folder, or to no folder (a link, a file).
    Replaced,
    Gone,
    /// It was stored to be read again.
    Again,
    /// A folder above it may no longer be searched.
    Denied,
    /// A mount starts at it or above it now.
    Mount,
    /// A network mount starts at it or above it now.
    NetworkMount,
    Failed(i32),
}

/// What a check found.
pub struct Checked {
    /// The folders to read again, by their place in what was checked, in
    /// that order.
    pub stale: Vec<(usize, Stale)>,
    pub took: Duration,
}

/// What proves stored folders current for a refresh: [`check`], or one
/// that stats them by other means (on NTFS, the system service's volume's
/// file table: `mft::Table::check`).
pub type Checker<'a> = &'a dyn Fn(&Root, &Stored, &Options, &Cancel) -> Result<Checked, Error>;

/// Prove the stored folders current where nothing watched: each folder
/// stat-ed by its path, its identity, mount and change time compared with
/// what was stored, on [`Options::threads`] threads. A folder at or below
/// a mount point the root's table holds is not stat-ed: read the table
/// again first ([`Root::reread_mounts`]). A path longer than the system
/// resolves is stat-ed through its parent's handle.
pub fn check(
    root: &Root,
    stored: &Stored,
    opts: &Options,
    cancel: &Cancel,
) -> Result<Checked, Error> {
    let start = Instant::now();
    let points = root.points();
    let stale = Mutex::new(Vec::new());
    shared(stored.len(), opts, cancel, |i| {
        if let Some(why) = sys::verdict(root, &points, &stored.rel(i), stored.folder(i)) {
            stale.lock().unwrap().push((i, why));
        }
    });
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let mut stale = stale.into_inner().unwrap();
    stale.sort_unstable_by_key(|&(i, _)| i);
    Ok(Checked {
        stale,
        took: start.elapsed(),
    })
}

/// The check, each folder watched first ([`Options::watcher`]): opened
/// by its path beneath the root, watched through its handle, then its
/// identity, mount and change time compared through that same handle. A
/// change made before the watch moves its change time; one made after is
/// heard. Gives each folder's watch beside the verdicts. Slower than
/// [`check`], which stats by path: an install pays it once.
pub fn check_watched(
    root: &Root,
    stored: &Stored,
    opts: &Options,
    cancel: &Cancel,
) -> Result<(Checked, Vec<Watch>), Error> {
    let start = Instant::now();
    let points = root.points();
    let stale = Mutex::new(Vec::new());
    let watches = Mutex::new(vec![Watch::Unasked; stored.len()]);
    shared(stored.len(), opts, cancel, |i| {
        let (why, watch) =
            sys::verdict_watched(root, &points, &stored.rel(i), stored.folder(i), opts);
        if let Some(why) = why {
            stale.lock().unwrap().push((i, why));
        }
        watches.lock().unwrap()[i] = watch;
    });
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let mut stale = stale.into_inner().unwrap();
    stale.sort_unstable_by_key(|&(i, _)| i);
    Ok((
        Checked {
            stale,
            took: start.elapsed(),
        },
        watches.into_inner().unwrap(),
    ))
}

/// Run `each` on every index below `len`, shared by `opts.threads`
/// threads in runs of 256, until done or cancelled, each at the priority
/// `opts` asks.
fn shared(len: usize, opts: &Options, cancel: &Cancel, each: impl Fn(usize) + Sync) {
    const RUN: usize = 256;
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..opts.threads.max(1) {
            scope.spawn(|| {
                if opts.background {
                    crate::pace::background();
                }
                while !cancel.is_cancelled() {
                    let from = next.fetch_add(RUN, Ordering::Relaxed);
                    if from >= len {
                        break;
                    }
                    for i in from..(from + RUN).min(len) {
                        each(i);
                    }
                }
            });
        }
    });
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) mod tests;
