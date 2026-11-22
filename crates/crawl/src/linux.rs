//! The crawl and the check on Linux: `getdents64` through rustix's `Dir`,
//! `statx` for identity, mount and times, `openat2` so that no link is
//! followed and no mount crossed.

use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, ResolveFlags, Statx, StatxFlags};
use rustix::io::Errno;

mod btrfs;
pub mod watch;

use crate::mounts::{self, Mount, Table, component};
use crate::{
    Attrs, Cancel, Crawled, Entry, Error, Exclusions, FileId, FolderStat, Kind, Listing, Options,
    Outcome, Time, Watch, Why,
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
///
/// Its folder is held open only while it is read ([`Root::let_go`] after):
/// a descriptor kept on a mount would make its user's `umount` fail. It is
/// opened again by its path when next read, and taken only if it is the
/// same folder, on the same mount.
pub struct Root {
    path: PathBuf,
    fd: std::sync::RwLock<Option<std::sync::Arc<OwnedFd>>>,
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
            return Err(Error::Kernel(
                "openat2 (Linux 5.6), or lets a system call filter refuse it",
            ));
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
            fd: std::sync::RwLock::new(Some(std::sync::Arc::new(fd))),
            stat,
            fs,
            table,
            below,
            exclusions,
        })
    }

    /// Its folder, open: as it was opened, or opened again by its path if
    /// let go of, refused (`EXDEV`) if that is no longer the folder it was
    /// (another folder, or its mount gone).
    fn dir(&self) -> rustix::io::Result<std::sync::Arc<OwnedFd>> {
        if let Some(fd) = self
            .fd
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            return Ok(std::sync::Arc::clone(fd));
        }
        let mut held = self
            .fd
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(fd) = held.as_ref() {
            return Ok(std::sync::Arc::clone(fd));
        }
        let fd = rustix::fs::open(&self.path, dir_flags(), Mode::empty())?;
        let st = statx_fd(&fd)?;
        if id_of(&st) != self.stat.id || st.stx_mnt_id != self.stat.mount {
            return Err(Errno::XDEV);
        }
        let fd = std::sync::Arc::new(fd);
        *held = Some(std::sync::Arc::clone(&fd));
        Ok(fd)
    }

    /// The access ACL of the folder at `rel`, if it is still the folder of
    /// identity `id`: opened beneath the root, following no link. None if
    /// it is not, has none, or cannot be read.
    pub fn access_acl(&self, rel: &Path, id: FileId) -> Option<crate::acl::Acl> {
        let fd = self.dir().and_then(|d| open_beneath(&d, rel)).ok()?;
        if id_of(&statx_fd(&fd).ok()?) != id {
            return None;
        }
        let mut buf = vec![0u8; 4096];
        let len = match rustix::fs::fgetxattr(&fd, "system.posix_acl_access", &mut buf[..]) {
            Ok(len) => len,
            Err(Errno::RANGE) => {
                buf.resize(1 << 16, 0);
                rustix::fs::fgetxattr(&fd, "system.posix_acl_access", &mut buf[..]).ok()?
            }
            Err(_) => return None,
        };
        crate::acl::Acl::parse(&buf[..len])
    }

    /// Let go of its folder until it is next read: between reads, nothing of
    /// its filesystem is held open.
    pub fn let_go(&self) {
        *self
            .fd
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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

    /// Take `exclusions` from now on: a folder they name is not read again.
    pub fn set_exclusions(&mut self, exclusions: Exclusions) {
        self.exclusions = exclusions;
    }

    /// Where mounts start below it, by path below it, as its table last
    /// said.
    pub fn mounts_below(&self) -> Vec<PathBuf> {
        self.below.keys().cloned().collect()
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

/// Whether the open folder `fd` has an access ACL: one more than its mode
/// says. A filesystem without ACLs has none; one that cannot be asked
/// says it may, so that the folder is never answered as its mode alone
/// says.
fn has_access_acl(fd: &OwnedFd) -> bool {
    let mut none: [u8; 0] = [];
    match rustix::fs::fgetxattr(fd, "system.posix_acl_access", &mut none[..]) {
        Ok(len) => len > 0,
        // NOTSUP and OPNOTSUPP are one number on Linux.
        Err(Errno::NODATA | Errno::NOTSUP) => false,
        Err(_) => true,
    }
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

/// A listing's names, `.` and `..` left out, in one buffer, each kept
/// with its terminator and with what the listing says of it: no
/// allocation for each name, a large folder's read again being mostly
/// its entries (DESIGN 8).
#[derive(Default)]
struct Names {
    bytes: Vec<u8>,
    /// Each name's range in `bytes`, its terminator included, its kind and
    /// its inode.
    at: Vec<(u32, u32, FileType, u64)>,
}

impl Names {
    fn push(&mut self, name: &CStr, kind: FileType, ino: u64) {
        if matches!(name.to_bytes(), b"." | b"..") {
            return;
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(name.to_bytes_with_nul());
        // A folder's names fit in 4 GiB: a name is at most 255 bytes.
        #[allow(clippy::cast_possible_truncation)]
        self.at
            .push((start as u32, self.bytes.len() as u32, kind, ino));
    }

    /// In the order of the names' bytes: the terminator, the least byte,
    /// orders a name before the longer ones it starts.
    fn sort(&mut self) {
        let bytes = &self.bytes;
        self.at.sort_unstable_by(|a, b| {
            bytes[a.0 as usize..a.1 as usize].cmp(&bytes[b.0 as usize..b.1 as usize])
        });
    }

    /// Each name's bytes, its terminator kept beside them for a stat (a
    /// name made a C string only where one is stat-ed: checking each for
    /// the one terminator costs a scan of it), its kind and its inode.
    fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8], FileType, u64)> {
        self.at.iter().map(|&(start, end, kind, ino)| {
            let with_nul = &self.bytes[start as usize..end as usize];
            (&with_nul[..with_nul.len() - 1], with_nul, kind, ino)
        })
    }
}

/// The names of the folder open as `fd`, read from its start in place
/// (`getdents64` through [`RawDir`]): none allocated for each entry. A
/// listing that fails part-way gives its error alone.
fn names_of(fd: impl AsFd) -> Result<Names, i32> {
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut raw = RawDir::new(fd, buf.spare_capacity_mut());
    let mut names = Names::default();
    while let Some(entry) = raw.next() {
        let entry = entry.map_err(Errno::raw_os_error)?;
        names.push(entry.file_name(), entry.file_type(), entry.ino());
    }
    Ok(names)
}

/// Read the folder at `rel` below the root: its stat first, then its
/// entries, each stat-ed through its handle (all of them with
/// [`Options::attrs`], folders alone without). Gives its stat where it
/// was taken, and what came of it.
pub fn read_folder(root: &Root, rel: &Path, opts: &Options) -> (Option<FolderStat>, Outcome) {
    read(root, rel, opts, &mut Watch::Unasked, None)
}

fn read(
    root: &Root,
    rel: &Path,
    opts: &Options,
    watch: &mut Watch,
    reuse: Option<Reuse<'_>>,
) -> (Option<FolderStat>, Outcome) {
    let not_covered = |stat, why| (stat, Outcome::NotCovered(why));
    if root.excluded_by_path(rel) {
        return not_covered(None, Why::Excluded);
    }
    if let Some(network) = root.mount_at(rel) {
        return not_covered(None, mount_why(network));
    }
    let read_at = Time::now();
    let fd = match root.dir().and_then(|d| open_beneath(&d, rel)) {
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
    let mut stat = match statx_fd(&fd) {
        Ok(st) => folder_stat(&st),
        Err(e) => return not_covered(None, Why::Failed(e.raw_os_error())),
    };
    if opts.acls && has_access_acl(&fd) {
        stat.mode |= crate::ACL;
    }
    if root.exclusions.holds(Some(stat.id), &root.path.join(rel)) {
        return not_covered(Some(stat), Why::Excluded);
    }
    // A btrfs snapshot below the root (a read-only subvolume) is left out,
    // its name kept: each would be another copy of what it was taken of
    // (the user's choice, 2026-11-22). Named as a root, it is read.
    if !rel.as_os_str().is_empty()
        && stat.id.ino == btrfs::SUBVOLUME_TOP
        && root.fs == "btrfs"
        && btrfs::read_only_subvolume(fd.as_fd())
    {
        return not_covered(Some(stat), Why::Snapshot);
    }
    if stat.mount != root.stat.mount {
        return not_covered(Some(stat), Why::Mount);
    }
    if let Some(watcher) = &opts.watcher {
        *watch = watcher.watch(fd.as_fd(), &root.path.join(rel));
    }
    // Read in place, no name allocated for each entry; a listing that
    // fails part-way gives its error alone: what was read of it is not
    // kept.
    let names = match names_of(&fd) {
        Ok(names) => names,
        Err(errno) => return not_covered(Some(stat), Why::CutShort(errno)),
    };
    if names.iter().any(|(name, ..)| name == b".private") {
        return not_covered(Some(stat), Why::Private);
    }
    let fd = fd.as_fd();
    // Read within a tick of its last change, or with a change time ahead
    // of the clock: a second change may have left that time as it was.
    let again = read_at < stat.ctime.plus(opts.tick);
    // Stat-ed or reused in the order of their names: `reuse` is asked so,
    // and the listing is so.
    let mut names = names;
    names.sort();
    let mut listing = Listing::new(stat, read_at, again);
    for (name, with_nul, file_type, ino) in names.iter() {
        // Taken as it was: no event named it, and the listing gives it the
        // same inode and kind.
        if let Some(entry) = reuse.and_then(|r| r(name, ino, kind_of(file_type))) {
            listing.push(name, entry);
            continue;
        }
        let fallback = Entry {
            kind: kind_of(file_type),
            id: FileId {
                dev: stat.id.dev,
                ino,
            },
            attrs: None,
        };
        let entry = if !root.below.is_empty()
            && root.mount_at(&rel.join(OsStr::from_bytes(name))).is_some()
        {
            // A mount point: never stat-ed (it may be a dead server's).
            fallback
        } else if opts.attrs || matches!(file_type, FileType::Directory | FileType::Unknown) {
            let Ok(name) = CStr::from_bytes_with_nul(with_nul) else {
                continue;
            };
            match statx_at(fd, name) {
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
        listing.push(name, entry);
    }
    (Some(stat), Outcome::Listed(listing))
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
    let (stat, outcome) = read(root, rel, opts, &mut watch, Some(reuse));
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
    let (stat, outcome) = read(root, rel, opts, &mut watch, None);
    (stat, outcome, watch)
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
    let (stat, outcome, watch) = read_folder_watched(root, &job.rel, opts);
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
                watch: Watch::Unasked,
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
        self.paths.extend_from_slice(rel.as_os_str().as_bytes());
        self.folders.push((self.paths.len(), folder));
    }

    pub fn len(&self) -> usize {
        self.folders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    /// The `i`th folder's path below the root.
    pub fn rel(&self, i: usize) -> &Path {
        let start = if i == 0 { 0 } else { self.folders[i - 1].0 };
        Path::new(OsStr::from_bytes(&self.paths[start..self.folders[i].0]))
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
    let points: Vec<(&PathBuf, bool)> = root.below.iter().map(|(p, &n)| (p, n)).collect();
    let stale = Mutex::new(Vec::new());
    shared(stored.len(), opts.threads, cancel, |i| {
        if let Some(why) = verdict(root, &points, stored.rel(i), stored.folder(i)) {
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

fn verdict(
    root: &Root,
    points: &[(&PathBuf, bool)],
    rel: &Path,
    stored: StoredFolder,
) -> Option<Stale> {
    if let Some(&(_, network)) = points.iter().find(|(p, _)| rel.starts_with(p)) {
        return Some(if network {
            Stale::NetworkMount
        } else {
            Stale::Mount
        });
    }
    if stored.again {
        return Some(Stale::Again);
    }
    match stat_path(root, rel) {
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
    let points: Vec<(&PathBuf, bool)> = root.below.iter().map(|(p, &n)| (p, n)).collect();
    let stale = Mutex::new(Vec::new());
    let watches = Mutex::new(vec![Watch::Unasked; stored.len()]);
    shared(stored.len(), opts.threads, cancel, |i| {
        let (why, watch) = verdict_watched(root, &points, stored.rel(i), stored.folder(i), opts);
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

fn verdict_watched(
    root: &Root,
    points: &[(&PathBuf, bool)],
    rel: &Path,
    stored: StoredFolder,
    opts: &Options,
) -> (Option<Stale>, Watch) {
    if let Some(&(_, network)) = points.iter().find(|(p, _)| rel.starts_with(p)) {
        let why = if network {
            Stale::NetworkMount
        } else {
            Stale::Mount
        };
        return (Some(why), Watch::Unasked);
    }
    let fd = match root.dir().and_then(|d| open_beneath(&d, rel)) {
        Ok(fd) => fd,
        Err(e) => {
            let why = match e {
                Errno::NOENT | Errno::NOTDIR => Stale::Gone,
                Errno::ACCESS | Errno::PERM => Stale::Denied,
                Errno::LOOP | Errno::XDEV => Stale::Replaced,
                e => Stale::Failed(e.raw_os_error()),
            };
            return (Some(why), Watch::Unasked);
        }
    };
    let watch = opts.watcher.as_ref().map_or(Watch::Unasked, |w| {
        w.watch(fd.as_fd(), &root.path.join(rel))
    });
    let why = match statx_fd(&fd) {
        Err(e) => Some(Stale::Failed(e.raw_os_error())),
        Ok(st) if st.stx_mnt_id != root.stat.mount => Some(Stale::Mount),
        Ok(st) if id_of(&st) != stored.id => Some(Stale::Replaced),
        Ok(_) if stored.again => Some(Stale::Again),
        Ok(st) if time(st.stx_ctime) != stored.ctime => Some(Stale::Changed),
        Ok(_) => None,
    };
    (why, watch)
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
    root.dir()
        .and_then(|d| open_beneath(&d, parent))
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
