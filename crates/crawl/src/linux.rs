//! The crawl and the check on Linux: `getdents64` through rustix's `Dir`,
//! `statx` for identity, mount and times, `openat2` so that no link is
//! followed and no mount crossed.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags, Statx, StatxFlags};
use rustix::io::Errno;

use crate::mounts::{self, Mount, Table, component};
use crate::{
    Attrs, Cancel, Crawled, Entry, Error, Exclusions, FileId, FolderStat, Kind, Listing, Options,
    Outcome, Time, Why,
};

/// The longest path the system resolves at once, its final NUL included.
const PATH_MAX: usize = 4096;
/// The longest part of a path opened at once, well below `PATH_MAX`.
const PART: usize = 3072;

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

/// A path resolved beneath a folder: no link followed, no mount crossed.
fn resolve_flags() -> ResolveFlags {
    ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV
}

fn mask() -> StatxFlags {
    StatxFlags::BASIC_STATS | StatxFlags::MNT_ID
}

/// A root, opened: the folder a crawl reads below and a check proves.
pub struct Root {
    path: PathBuf,
    fd: OwnedFd,
    stat: FolderStat,
    fs: String,
    table: Table,
    /// Where mounts start below it, and whether each reaches a network.
    below: HashMap<PathBuf, bool>,
    exclusions: Exclusions,
}

impl Root {
    /// The folder at `path`, links in its path followed, with the mount
    /// table as it is now.
    pub fn open(path: &Path, exclusions: Exclusions) -> Result<Root, Error> {
        let table = Table::read().map_err(Error::Root)?;
        Root::open_with(path, exclusions, table)
    }

    /// The folder at `path`, with a mount table given (tests give one
    /// that holds a network mount).
    pub fn open_with(path: &Path, exclusions: Exclusions, table: Table) -> Result<Root, Error> {
        // Told from its spelling first, so that a root on a dead server is
        // never stat-ed; then again once resolved.
        let on_network = |p: &Path| table.holding(p).is_some_and(Mount::network);
        if mounts::lexical(path).is_some_and(|p| on_network(&p)) {
            return Err(Error::Network);
        }
        let path = std::fs::canonicalize(path).map_err(Error::Root)?;
        if on_network(&path) {
            return Err(Error::Network);
        }
        let fd = rustix::fs::open(&path, dir_flags(), Mode::empty())
            .map_err(|e| Error::Root(e.into()))?;
        let st = statx_fd(&fd).map_err(|e| Error::Root(e.into()))?;
        if st.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
            return Err(Error::Kernel("statx's mount id (Linux 5.8)"));
        }
        if let Err(Errno::NOSYS) = openat2(&fd, Path::new(".")) {
            return Err(Error::Kernel("openat2 (Linux 5.6)"));
        }
        let stat = folder_stat(&st);
        // By its mount's id too: a bind mount of a share is spelled as any
        // folder.
        if table.by_id(stat.mount).is_some_and(Mount::network) {
            return Err(Error::Network);
        }
        let fs = rustix::fs::fstatfs(&fd).map_or_else(|_| "unknown".into(), |s| fs_name(&s));
        let below = table.below(&path);
        Ok(Root {
            path,
            fd,
            stat,
            fs,
            table,
            below,
            exclusions,
        })
    }

    /// Its path, as the system resolves it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stat(&self) -> &FolderStat {
        &self.stat
    }

    /// Its filesystem's name (`btrfs`, `ext4`), from its magic number.
    pub fn fs_name(&self) -> &str {
        &self.fs
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    pub fn exclusions(&self) -> &Exclusions {
        &self.exclusions
    }

    /// Read the mount table again, as each check does first.
    pub fn reread_mounts(&mut self) -> std::io::Result<()> {
        self.set_table(Table::read()?);
        Ok(())
    }

    /// Take `table` as the mount table.
    pub fn set_table(&mut self, table: Table) {
        self.below = table.below(&self.path);
        self.table = table;
    }

    /// Whether a mount starts at `rel`, and if so whether it reaches a
    /// network.
    fn mount_at(&self, rel: &Path) -> Option<bool> {
        if self.below.is_empty() {
            return None;
        }
        self.below.get(rel).copied()
    }

    /// Whether `rel` or a folder above it, below the root, is excluded by
    /// its path.
    fn excluded_by_path(&self, rel: &Path) -> bool {
        rel.ancestors()
            .filter(|a| !a.as_os_str().is_empty())
            .any(|a| self.exclusions.holds(None, &self.path.join(a)))
    }
}

/// `openat2` beneath `dir`, tried again where a rename or a mount raced it.
fn openat2(dir: &impl AsFd, rel: &Path) -> rustix::io::Result<OwnedFd> {
    let mut tries = 0;
    loop {
        match rustix::fs::openat2(dir, rel, dir_flags(), Mode::empty(), resolve_flags()) {
            Err(Errno::AGAIN) if tries < 8 => tries += 1,
            other => return other,
        }
    }
}

/// The folder at `rel` below the root, opened by a path that holds no
/// link and crosses no mount, a part at a time where it is longer than
/// the system resolves at once.
fn open_beneath(root: &OwnedFd, rel: &Path) -> rustix::io::Result<OwnedFd> {
    if rel.as_os_str().is_empty() {
        return openat2(root, Path::new("."));
    }
    if rel.as_os_str().len() < PART {
        return openat2(root, rel);
    }
    let mut fd: Option<OwnedFd> = None;
    let mut part = PathBuf::new();
    for name in rel.components() {
        let name = name.as_os_str();
        if !part.as_os_str().is_empty() && part.as_os_str().len() + 1 + name.len() >= PART {
            let next = match &fd {
                Some(fd) => openat2(fd, &part)?,
                None => openat2(root, &part)?,
            };
            fd = Some(next);
            part = PathBuf::new();
        }
        part.push(name);
    }
    match &fd {
        Some(fd) => openat2(fd, &part),
        None => openat2(root, &part),
    }
}

fn statx_fd(fd: &impl AsFd) -> rustix::io::Result<Statx> {
    rustix::fs::statx(fd, c"", AtFlags::EMPTY_PATH, mask())
}

/// An entry's stat through its folder's handle, no link followed.
fn statx_at(dir: BorrowedFd<'_>, name: &CStr) -> rustix::io::Result<Statx> {
    rustix::fs::statx(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC,
        mask(),
    )
}

fn id_of(st: &Statx) -> FileId {
    FileId {
        dev: rustix::fs::makedev(st.stx_dev_major, st.stx_dev_minor),
        ino: st.stx_ino,
    }
}

fn time(ts: rustix::fs::StatxTimestamp) -> Time {
    Time {
        sec: ts.tv_sec,
        nsec: ts.tv_nsec,
    }
}

fn kind_of(file_type: FileType) -> Kind {
    match file_type {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Folder,
        FileType::Symlink => Kind::Link,
        _ => Kind::Other,
    }
}

fn kind_of_mode(mode: u16) -> Kind {
    kind_of(FileType::from_raw_mode(u32::from(mode)))
}

fn folder_stat(st: &Statx) -> FolderStat {
    FolderStat {
        id: id_of(st),
        mount: st.stx_mnt_id,
        ctime: time(st.stx_ctime),
        mtime: time(st.stx_mtime),
        owner: st.stx_uid,
        group: st.stx_gid,
        mode: st.stx_mode & 0o7777,
    }
}

fn attrs_of(st: &Statx) -> Attrs {
    Attrs {
        size: st.stx_size,
        alloc: st.stx_blocks.saturating_mul(512),
        mtime: time(st.stx_mtime),
        ctime: time(st.stx_ctime),
    }
}

/// Why a folder could not be opened.
fn why_not_opened(e: Errno) -> Why {
    match e {
        Errno::ACCESS | Errno::PERM => Why::Denied,
        Errno::NOENT => Why::Gone,
        // A link or a file where a folder of its path was.
        Errno::LOOP | Errno::NOTDIR => Why::Replaced,
        // A mount made on its path since the table was read.
        Errno::XDEV => Why::Mount,
        e => Why::Failed(e.raw_os_error()),
    }
}

/// A name of a listing, with what the listing says of it.
type Named = (CString, FileType, u64);

/// Every name of a listing, `.` and `..` left out. A listing that fails
/// part-way gives its error alone: what was read of it is not kept.
fn names_of(entries: impl Iterator<Item = rustix::io::Result<Named>>) -> Result<Vec<Named>, i32> {
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Errno::raw_os_error)?;
        if !matches!(entry.0.to_bytes(), b"." | b"..") {
            names.push(entry);
        }
    }
    Ok(names)
}

/// Read the folder at `rel` below the root: its stat first, then its
/// entries, each stat-ed through its handle (all of them with
/// [`Options::attrs`], folders alone without). Gives its stat where it
/// was taken, and what came of it.
pub fn read_folder(root: &Root, rel: &Path, opts: &Options) -> (Option<FolderStat>, Outcome) {
    let not_covered = |stat, why| (stat, Outcome::NotCovered(why));
    if root.excluded_by_path(rel) {
        return not_covered(None, Why::Excluded);
    }
    if let Some(network) = root.mount_at(rel) {
        return not_covered(None, mount_why(network));
    }
    let read_at = Time::now();
    let fd = match open_beneath(&root.fd, rel) {
        Ok(fd) => fd,
        Err(e) => {
            // Its stat where its parent may still be searched, so the next
            // check can try it again; none where its path is not its own.
            let why = why_not_opened(e);
            let stat = matches!(why, Why::Denied | Why::Failed(_))
                .then(|| stat_path(root, rel).ok())
                .flatten()
                .map(|st| folder_stat(&st));
            return not_covered(stat, why);
        }
    };
    let stat = match statx_fd(&fd) {
        Ok(st) => folder_stat(&st),
        Err(e) => return not_covered(None, Why::Failed(e.raw_os_error())),
    };
    if root.exclusions.holds(Some(stat.id), &root.path.join(rel)) {
        return not_covered(Some(stat), Why::Excluded);
    }
    if stat.mount != root.stat.mount {
        return not_covered(Some(stat), Why::Mount);
    }
    let mut dir = match Dir::new(fd) {
        Ok(dir) => dir,
        Err(e) => return not_covered(Some(stat), Why::Failed(e.raw_os_error())),
    };
    let listed = std::iter::from_fn(|| {
        dir.read()
            .map(|entry| entry.map(|e| (e.file_name().to_owned(), e.file_type(), e.ino())))
    });
    let names = match names_of(listed) {
        Ok(names) => names,
        Err(errno) => return not_covered(Some(stat), Why::CutShort(errno)),
    };
    if names
        .iter()
        .any(|(name, ..)| name.to_bytes() == b".private")
    {
        return not_covered(Some(stat), Why::Private);
    }
    let Ok(fd) = dir.fd() else {
        return not_covered(Some(stat), Why::Failed(Errno::BADF.raw_os_error()));
    };
    // Read within a tick of its last change, or with a change time ahead
    // of the clock: a second change may have left that time as it was.
    let again = read_at < stat.ctime.plus(opts.tick);
    let mut listing = Listing::new(stat, read_at, again);
    for (name, file_type, ino) in names {
        let fallback = Entry {
            kind: kind_of(file_type),
            id: FileId {
                dev: stat.id.dev,
                ino,
            },
            attrs: None,
        };
        let entry = if !root.below.is_empty()
            && root
                .mount_at(&rel.join(OsStr::from_bytes(name.to_bytes())))
                .is_some()
        {
            // A mount point: never stat-ed (it may be a dead server's).
            fallback
        } else if opts.attrs || matches!(file_type, FileType::Directory | FileType::Unknown) {
            match statx_at(fd, &name) {
                Ok(st) => Entry {
                    kind: kind_of_mode(st.stx_mode),
                    id: id_of(&st),
                    attrs: opts.attrs.then(|| attrs_of(&st)),
                },
                // Gone since the listing, which moved the folder's change
                // time: the next check reads it again.
                Err(Errno::NOENT) => continue,
                Err(_) => fallback,
            }
        } else {
            fallback
        };
        listing.push(name.to_bytes(), entry);
    }
    (Some(stat), Outcome::Listed(listing))
}

fn mount_why(network: bool) -> Why {
    if network {
        Why::NetworkMount
    } else {
        Why::Mount
    }
}

/// A folder waiting to be read.
struct Job {
    slot: usize,
    rel: PathBuf,
    parent: Option<usize>,
}

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
    let start = Instant::now();
    let next_slot = AtomicUsize::new(1);
    let stopped = AtomicBool::new(false);
    let queue = Queue {
        state: Mutex::new((
            vec![Job {
                slot: 0,
                rel: from.to_path_buf(),
                parent: None,
            }],
            0,
        )),
        turn: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..opts.threads.max(1) {
            scope.spawn(|| {
                while let Some(job) = queue.next() {
                    if cancel.is_cancelled() {
                        stopped.store(true, Ordering::Relaxed);
                        queue.done(Vec::new());
                        continue;
                    }
                    let found = visit(root, job, opts, &next_slot, sink);
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
) -> Vec<Job> {
    let (stat, outcome) = read_folder(root, &job.rel, opts);
    let mut found = Vec::new();
    if let Outcome::Listed(listing) = &outcome {
        for (name, entry) in listing.iter() {
            if entry.kind != Kind::Folder {
                continue;
            }
            let rel = job.rel.join(component(name));
            let slot = next_slot.fetch_add(1, Ordering::Relaxed);
            let not_read = |why| Crawled {
                rel: rel.clone(),
                parent: Some(job.slot),
                stat: None,
                outcome: Outcome::NotCovered(why),
            };
            if let Some(network) = root.mount_at(&rel) {
                sink(slot, not_read(mount_why(network)));
            } else if root.exclusions.holds(Some(entry.id), &root.path.join(&rel)) {
                sink(slot, not_read(Why::Excluded));
            } else {
                found.push(Job {
                    slot,
                    rel,
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
    let slots: Mutex<Vec<Option<Crawled>>> = Mutex::new(Vec::new());
    let took = crawl(root, from, opts, cancel, &|slot, crawled| {
        let mut slots = slots.lock().unwrap();
        if slots.len() <= slot {
            slots.resize_with(slot + 1, || None);
        }
        slots[slot] = Some(crawled);
    })?;
    let folders = slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|c| c.expect("a crawl hands every slot it gives"))
        .collect();
    Ok(Crawl { folders, took })
}

/// A folder as the index stored it, for the check.
#[derive(Clone, Debug)]
pub struct Stored {
    /// Its path below the root.
    pub rel: PathBuf,
    pub id: FileId,
    pub ctime: Time,
    /// Read it again whatever its times say: it was read within a tick of
    /// its last change, or its read failed.
    pub again: bool,
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

/// Prove the stored folders current where nothing watched: each folder
/// stat-ed by its path, its identity, mount and change time compared with
/// what was stored, on [`Options::threads`] threads. A folder at or below
/// a mount point the root's table holds is not stat-ed: read the table
/// again first ([`Root::reread_mounts`]). A path longer than the system
/// resolves is stat-ed through its parent's handle.
pub fn check(
    root: &Root,
    stored: &[Stored],
    opts: &Options,
    cancel: &Cancel,
) -> Result<Checked, Error> {
    let start = Instant::now();
    let points: Vec<(&PathBuf, bool)> = root.below.iter().map(|(p, &n)| (p, n)).collect();
    let stale = Mutex::new(Vec::new());
    shared(stored.len(), opts.threads, cancel, |i| {
        if let Some(why) = verdict(root, &points, &stored[i]) {
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

fn verdict(root: &Root, points: &[(&PathBuf, bool)], stored: &Stored) -> Option<Stale> {
    if let Some(&(_, network)) = points.iter().find(|(p, _)| stored.rel.starts_with(p)) {
        return Some(if network {
            Stale::NetworkMount
        } else {
            Stale::Mount
        });
    }
    if stored.again {
        return Some(Stale::Again);
    }
    match stat_path(root, &stored.rel) {
        Err(Errno::NOENT | Errno::NOTDIR) => Some(Stale::Gone),
        Err(Errno::ACCESS | Errno::PERM) => Some(Stale::Denied),
        Err(Errno::LOOP | Errno::XDEV) => Some(Stale::Replaced),
        Err(e) => Some(Stale::Failed(e.raw_os_error())),
        Ok(st) if kind_of_mode(st.stx_mode) != Kind::Folder => Some(Stale::Replaced),
        Ok(st) if st.stx_mnt_id != root.stat.mount => Some(Stale::Mount),
        Ok(st) if id_of(&st) != stored.id => Some(Stale::Replaced),
        Ok(st) if time(st.stx_ctime) != stored.ctime => Some(Stale::Changed),
        Ok(_) => None,
    }
}

/// The stat of the folder at `rel` by its path, as phase 0 found fastest,
/// no link followed at its end: through its parent's handle where the
/// path is longer than the system resolves at once.
fn stat_path(root: &Root, rel: &Path) -> rustix::io::Result<Statx> {
    let path = root.path.join(rel);
    if path.as_os_str().len() < PATH_MAX {
        return rustix::fs::statx(rustix::fs::CWD, &path, AtFlags::SYMLINK_NOFOLLOW, mask());
    }
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = rel.file_name().unwrap_or_default();
    open_beneath(&root.fd, parent)
        .and_then(|fd| rustix::fs::statx(&fd, name, AtFlags::SYMLINK_NOFOLLOW, mask()))
}

/// Run `each` on every index below `len`, shared by `threads` threads in
/// runs of 256, until done or cancelled.
fn shared(len: usize, threads: usize, cancel: &Cancel, each: impl Fn(usize) + Sync) {
    const RUN: usize = 256;
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
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

/// A filesystem's name by its `statfs` magic.
fn fs_name(fs: &rustix::fs::StatFs) -> String {
    #[allow(clippy::unnecessary_cast)] // its type differs between architectures
    filesystem_name(fs.f_type as u64)
}

/// A filesystem's name by Linux's `statfs` magic number, or the number.
pub fn filesystem_name(magic: u64) -> String {
    match magic {
        0x9123_683e => "btrfs".into(),
        0xef53 => "ext4".into(),
        0x5846_5342 => "xfs".into(),
        0x2fc1_2fc1 => "zfs".into(),
        0xf2f5_2010 => "f2fs".into(),
        0xca45_1a4e => "bcachefs".into(),
        0x0102_1994 => "tmpfs".into(),
        0x794c_7630 => "overlayfs".into(),
        0x6573_5546 => "fuse".into(),
        0x6969 => "nfs".into(),
        0xff53_4d42 | 0xfe53_4d42 => "smb".into(),
        0 => "unknown".into(),
        other => format!("{other:#x}"),
    }
}

#[cfg(test)]
mod tests;
