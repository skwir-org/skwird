//! The crawl and the check on macOS (phase 5): a folder opened beneath
//! its root by a path that may hold no link (`O_NOFOLLOW_ANY`), listed in
//! batches that give each entry's kind, identity and times with its name
//! (`getattrlistbulk`), so that no entry is stat-ed, fsearch's walk.
//!
//! What Linux has that macOS has not, and what stands for it:
//! - no `RESOLVE_BENEATH` nor `RESOLVE_NO_XDEV`: a path is beneath its
//!   root as the crawl builds it, of the names its listings gave, never
//!   `..`; a folder opened is on its root's device, or it is a mount;
//! - no mount id: a mount is told by its device, which each mount has of
//!   its own;
//! - `PATH_MAX` is 1,024 bytes: a longer path is opened a part at a time.

// getattrlistbulk and getfsstat, which rustix has no binding for.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::mounts::{Mount, Table};
use crate::walk::{Reuse, Stale, StoredFolder, mount_why};
use crate::{
    Attrs, Entry, Error, Exclusions, FileId, FolderStat, Kind, Listing, Options, Outcome, Time,
    Watch, Why,
};

/// The longest path the system resolves at once, its final NUL included.
const PATH_MAX: usize = 1024;
/// The longest part of a path opened at once, well below `PATH_MAX`.
const PART: usize = 768;
/// `fstatat`'s refusal of a link anywhere in a path (macOS 11), which
/// rustix does not name.
const SYMLINK_NOFOLLOW_ANY: u32 = 0x0800;
/// An entry's error, said in its listing (`getattrlistbulk`), which libc
/// does not name.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// What `getattrlistbulk` is given to fill: its entries, a batch at a
/// time.
const BATCH: usize = 64 * 1024;

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

/// A folder opened by a path that may hold no link at all, its last name
/// among them. In place of `O_NOFOLLOW`, never beside it: macOS refuses
/// the two together (`EINVAL`), as the first runner found.
fn beneath_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW_ANY
}

/// A root, opened: the folder a crawl reads below and a check proves.
///
/// Its folder is held open only while it is read ([`Root::let_go`] after):
/// a descriptor kept on a volume would keep it from being ejected. It is
/// opened again by its path when next read, and taken only if it is the
/// same folder, on the same device.
pub struct Root {
    path: PathBuf,
    fd: std::sync::RwLock<Option<std::sync::Arc<OwnedFd>>>,
    stat: FolderStat,
    fs: String,
    table: Table,
    /// Where mounts start below it, and whether each reaches a network.
    below: std::collections::HashMap<PathBuf, bool>,
    exclusions: Exclusions,
}

impl Root {
    /// The folder at `path`, links in its path followed, with the mount
    /// table as it is now.
    pub fn open(path: &Path, exclusions: Exclusions) -> Result<Root, Error> {
        let table = Table::read().map_err(Error::Root)?;
        Root::open_with(path, exclusions, table)
    }

    /// The folder at `path`, with a mount table given.
    pub fn open_with(path: &Path, exclusions: Exclusions, table: Table) -> Result<Root, Error> {
        // Told from its spelling first, so that a root on a dead server is
        // never stat-ed; then again once resolved.
        let on_network = |p: &Path| table.holding(p).is_some_and(Mount::network);
        if crate::mounts::lexical(path).is_some_and(|p| on_network(&p)) {
            return Err(Error::Network);
        }
        let path = std::fs::canonicalize(path).map_err(Error::Root)?;
        if on_network(&path) {
            return Err(Error::Network);
        }
        let fd = rustix::fs::open(&path, dir_flags(), Mode::empty())
            .map_err(|e| Error::Root(e.into()))?;
        let st = rustix::fs::fstat(&fd).map_err(|e| Error::Root(e.into()))?;
        let fs = rustix::fs::fstatfs(&fd).map_err(|e| Error::Root(e.into()))?;
        // By what its own volume says too: the system knows which reach a
        // network (`MNT_LOCAL`).
        if fs.f_flags & local_flag() == 0 {
            return Err(Error::Network);
        }
        let below = table.below(&path);
        Ok(Root {
            path,
            fd: std::sync::RwLock::new(Some(std::sync::Arc::new(fd))),
            stat: folder_stat(&st),
            fs: name_of(&fs.f_fstypename),
            table,
            below,
            exclusions,
        })
    }

    /// Its folder, open: as it was opened, or opened again by its path if
    /// let go of, refused (`EXDEV`) if that is no longer the folder it was
    /// (another folder, or its volume gone).
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
        if folder_stat(&rustix::fs::fstat(&fd)?).id != self.stat.id {
            return Err(Errno::XDEV);
        }
        let fd = std::sync::Arc::new(fd);
        *held = Some(std::sync::Arc::clone(&fd));
        Ok(fd)
    }

    /// No POSIX access ACL on macOS: its ACLs are extended ones, which a
    /// user's engine does not judge by (the system service's, later in
    /// phase 5).
    #[allow(clippy::unused_self)] // every system's Root answers it
    pub fn access_acl(&self, _: &Path, _: FileId) -> Option<crate::acl::Acl> {
        None
    }

    /// Let go of its folder until it is next read: between reads, nothing of
    /// its volume is held open.
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

    /// Its filesystem's name (`apfs`, `hfs`), as its volume says it.
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
    pub(crate) fn mount_at(&self, rel: &Path) -> Option<bool> {
        if self.below.is_empty() {
            return None;
        }
        self.below.get(rel).copied()
    }

    /// Where mounts start below it, and whether each reaches a network:
    /// what the check stats no folder at or below.
    pub(crate) fn points(&self) -> Vec<(PathBuf, bool)> {
        self.below.iter().map(|(p, &n)| (p.clone(), n)).collect()
    }

    /// Whether `rel` or a folder above it, below the root, is excluded by
    /// its path.
    fn excluded_by_path(&self, rel: &Path) -> bool {
        rel.ancestors()
            .filter(|a| !a.as_os_str().is_empty())
            .any(|a| self.exclusions.holds(None, &self.path.join(a)))
    }
}

/// `MNT_LOCAL`, as `statfs` gives its flags.
#[allow(clippy::cast_sign_loss)] // a flag's bits
fn local_flag() -> u32 {
    libc::MNT_LOCAL as u32
}

/// A name the system gives in a fixed array, up to its NUL.
fn name_of(chars: &[libc::c_char]) -> String {
    String::from_utf8_lossy(&bytes_of(chars)).into_owned()
}

#[allow(clippy::cast_sign_loss)] // a C char's bits
fn bytes_of(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect()
}

/// A device number, as macOS gives it signed, as the identities and mounts
/// of every system keep it.
#[allow(clippy::cast_sign_loss)] // a device number's bits
fn dev(dev: libc::dev_t) -> u64 {
    u64::from(dev as u32)
}

/// The system's mount table (`getfsstat`), as it holds it now: asked
/// without waiting on any volume (`MNT_NOWAIT`), a dead server's among
/// them.
pub(crate) fn mounts() -> std::io::Result<Vec<Mount>> {
    // SAFETY: no buffer given: the count of mounts alone.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(count).map_err(|_| std::io::Error::last_os_error())?;
    // Room for mounts made since the count.
    let mut all: Vec<libc::statfs> = Vec::with_capacity(count + 16);
    let size = i32::try_from(all.capacity() * std::mem::size_of::<libc::statfs>())
        .map_err(|_| std::io::Error::other("too many mounts"))?;
    // SAFETY: a buffer of `size` bytes, its capacity, which the call fills
    // with whole entries alone, and says how many.
    let got = unsafe { libc::getfsstat(all.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    let got = usize::try_from(got).map_err(|_| std::io::Error::last_os_error())?;
    // SAFETY: the call filled the first `got` entries, at most its capacity.
    unsafe { all.set_len(got.min(all.capacity())) };
    Ok(all
        .iter()
        .map(|s| {
            // SAFETY: an fsid is two 32-bit numbers, whose first is its
            // volume's device (libc keeps them private).
            let fsid: [i32; 2] = unsafe { std::mem::transmute(s.f_fsid) };
            let device = dev(fsid[0]);
            Mount {
                id: device,
                parent: 0,
                #[allow(clippy::cast_sign_loss)] // a device number's parts
                dev: (
                    libc::major(fsid[0]) as u32,
                    libc::minor(fsid[0]) as u32,
                ),
                root: PathBuf::from("/"),
                point: PathBuf::from(OsStr::from_bytes(&bytes_of(&s.f_mntonname))),
                fstype: name_of(&s.f_fstypename),
                source: OsStr::from_bytes(&bytes_of(&s.f_mntfromname)).to_owned(),
                local: s.f_flags & local_flag() != 0,
            }
        })
        .collect())
}

/// The folder at `rel` below the root, opened by a path that may hold no
/// link, a part at a time where it is longer than the system resolves at
/// once. A mount crossed is told by the folder's device, by the caller.
fn open_beneath(root: &OwnedFd, rel: &Path) -> rustix::io::Result<OwnedFd> {
    if rel.as_os_str().is_empty() {
        return rustix::fs::openat(root, ".", beneath_flags(), Mode::empty());
    }
    if rel.as_os_str().len() < PART {
        return rustix::fs::openat(root, rel, beneath_flags(), Mode::empty());
    }
    let mut fd: Option<OwnedFd> = None;
    let mut part = PathBuf::new();
    for name in rel.components() {
        let name = name.as_os_str();
        if !part.as_os_str().is_empty() && part.as_os_str().len() + 1 + name.len() >= PART {
            let next = match &fd {
                Some(fd) => rustix::fs::openat(fd, &part, beneath_flags(), Mode::empty())?,
                None => rustix::fs::openat(root, &part, beneath_flags(), Mode::empty())?,
            };
            fd = Some(next);
            part = PathBuf::new();
        }
        part.push(name);
    }
    match &fd {
        Some(fd) => rustix::fs::openat(fd, &part, beneath_flags(), Mode::empty()),
        None => rustix::fs::openat(root, &part, beneath_flags(), Mode::empty()),
    }
}

/// An instant as the system's `timespec` gives it.
fn time(sec: i64, nsec: i64) -> Time {
    Time {
        sec,
        nsec: u32::try_from(nsec.clamp(0, 999_999_999)).unwrap_or(0),
    }
}

fn kind_of_mode(mode: u16) -> Kind {
    match FileType::from_raw_mode(mode) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Folder,
        FileType::Symlink => Kind::Link,
        _ => Kind::Other,
    }
}

fn folder_stat(st: &Stat) -> FolderStat {
    FolderStat {
        id: FileId {
            dev: dev(st.st_dev),
            ino: st.st_ino,
        },
        mount: dev(st.st_dev),
        ctime: time(st.st_ctime, st.st_ctime_nsec),
        mtime: time(st.st_mtime, st.st_mtime_nsec),
        owner: st.st_uid,
        group: st.st_gid,
        mode: st.st_mode & 0o7777,
    }
}

#[allow(clippy::cast_sign_loss)] // sizes and counts the system keeps signed
fn attrs_of(st: &Stat) -> Attrs {
    Attrs {
        size: st.st_size as u64,
        alloc: (st.st_blocks as u64).saturating_mul(512),
        mtime: time(st.st_mtime, st.st_mtime_nsec),
        ctime: time(st.st_ctime, st.st_ctime_nsec),
        links: u32::from(st.st_nlink),
    }
}

/// Why a folder could not be opened.
fn why_not_opened(e: Errno) -> Why {
    match e {
        Errno::ACCESS | Errno::PERM => Why::Denied,
        Errno::NOENT => Why::Gone,
        // A link or a file where a folder of its path was.
        Errno::LOOP | Errno::NOTDIR => Why::Replaced,
        Errno::XDEV => Why::Mount,
        e => Why::Failed(e.raw_os_error()),
    }
}

/// One entry, as its folder's listing gave it.
struct Listed {
    name: Vec<u8>,
    kind: Kind,
    id: FileId,
    attrs: Option<Attrs>,
    /// A volume is mounted on it, as its listing says.
    mount_point: bool,
}

/// What a listing asks of each entry: its name, kind and identity, its
/// error if it has one, whether a folder is a mount point, and with
/// `attrs` its times, size, allocation and count of names.
fn asked(attrs: bool) -> libc::attrlist {
    let times = if attrs {
        libc::ATTR_CMN_MODTIME | libc::ATTR_CMN_CHGTIME
    } else {
        0
    };
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | times
            | libc::ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: libc::ATTR_DIR_MOUNTSTATUS,
        fileattr: if attrs {
            libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH
        } else {
            0
        },
        forkattr: 0,
    }
}

/// An entry's bytes, read field after field as the system packs them:
/// each where the one before ends, in the order of their bits.
struct Fields<'a> {
    b: &'a [u8],
    at: usize,
}

impl Fields<'_> {
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_ne_bytes(self.b.get(self.at..self.at + 4)?.try_into().ok()?);
        self.at += 4;
        Some(v)
    }

    fn u64(&mut self) -> Option<u64> {
        let v = u64::from_ne_bytes(self.b.get(self.at..self.at + 8)?.try_into().ok()?);
        self.at += 8;
        Some(v)
    }

    fn i32(&mut self) -> Option<i32> {
        let v = i32::from_ne_bytes(self.b.get(self.at..self.at + 4)?.try_into().ok()?);
        self.at += 4;
        Some(v)
    }

    fn i64(&mut self) -> Option<i64> {
        let v = i64::from_ne_bytes(self.b.get(self.at..self.at + 8)?.try_into().ok()?);
        self.at += 8;
        Some(v)
    }

    fn time(&mut self) -> Option<Time> {
        Some(time(self.i64()?, self.i64()?))
    }

    /// A name, by the reference at this field: its offset from the
    /// reference, and its length with its NUL.
    fn name(&mut self) -> Option<Vec<u8>> {
        let here = self.at;
        let offset = self.i32()?;
        let len = usize::try_from(self.u32()?).ok()?;
        let start = here.checked_add_signed(isize::try_from(offset).ok()?)?;
        let with_nul = self.b.get(start..start.checked_add(len)?)?;
        Some(with_nul.strip_suffix(&[0]).unwrap_or(with_nul).to_vec())
    }
}

/// An entry of a listing's batch: none where its bytes do not hold one,
/// `Err` its entry's name and error where the system could not give its
/// attributes, for a stat of it instead.
fn parse(b: &[u8], wanted: &libc::attrlist) -> Option<Result<Listed, Vec<u8>>> {
    let mut f = Fields { b, at: 4 };
    let common = f.u32()?;
    let _vol = f.u32()?;
    let dir = f.u32()?;
    let file = f.u32()?;
    let _fork = f.u32()?;
    let error = if common & ATTR_CMN_ERROR == 0 {
        0
    } else {
        f.u32()?
    };
    if common & libc::ATTR_CMN_NAME == 0 {
        return None;
    }
    let name = f.name()?;
    // Each asked of it but those a kind has not: a folder's file
    // attributes, a file's folder's.
    let lacking = |got: u32, want: u32| got & want != want;
    let dev = (common & libc::ATTR_CMN_DEVID != 0)
        .then(|| f.u32())
        .flatten();
    let objtype = (common & libc::ATTR_CMN_OBJTYPE != 0)
        .then(|| f.u32())
        .flatten();
    let mtime = (common & libc::ATTR_CMN_MODTIME != 0)
        .then(|| f.time())
        .flatten();
    let ctime = (common & libc::ATTR_CMN_CHGTIME != 0)
        .then(|| f.time())
        .flatten();
    let ino = (common & libc::ATTR_CMN_FILEID != 0)
        .then(|| f.u64())
        .flatten();
    let common_wanted = wanted.commonattr & !(ATTR_CMN_ERROR | libc::ATTR_CMN_RETURNED_ATTRS);
    let (Some(dev), Some(objtype), Some(ino)) = (dev, objtype, ino) else {
        return Some(Err(name));
    };
    if error != 0 || lacking(common, common_wanted) {
        return Some(Err(name));
    }
    let kind = match objtype {
        1 => Kind::File,
        2 => Kind::Folder,
        5 => Kind::Link,
        _ => Kind::Other,
    };
    let mut mount_point = false;
    if dir & libc::ATTR_DIR_MOUNTSTATUS != 0 {
        mount_point = f.u32()? & libc::DIR_MNTSTATUS_MNTPOINT != 0;
    }
    let mut links = 0;
    let mut alloc = 0;
    let mut size = 0;
    if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        links = f.u32()?;
    }
    if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        alloc = f.u64()?;
    }
    if file & libc::ATTR_FILE_DATALENGTH != 0 {
        size = f.u64()?;
    }
    let attrs = match (mtime, ctime) {
        (Some(mtime), Some(ctime)) if kind == Kind::Folder || !lacking(file, wanted.fileattr) => {
            Some(Attrs {
                size,
                alloc,
                mtime,
                ctime,
                links,
            })
        }
        // Asked, and not given: a stat gives them.
        _ if wanted.fileattr != 0 => return Some(Err(name)),
        _ => None,
    };
    let id = FileId {
        dev: u64::from(dev),
        ino,
    };
    Some(Ok(Listed {
        name,
        kind,
        id,
        attrs,
        mount_point,
    }))
}

thread_local! {
    /// Each thread's batch, filled anew for each folder it lists.
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0; BATCH]);
}

/// The entries of the folder open as `fd`, from its start, each with what
/// its listing gave of it (`getattrlistbulk`). An entry whose attributes
/// the listing could not give is stat-ed (gone since, left out). A listing
/// or a stat that fails gives its error alone.
fn entries_of(fd: BorrowedFd<'_>, attrs: bool) -> Result<Vec<Listed>, i32> {
    let mut wanted = asked(attrs);
    let mut out = Vec::new();
    let mut unparsed = Vec::new();
    BUF.with_borrow_mut(|buf| {
        loop {
            // SAFETY: the folder's descriptor, open; what is asked, ours;
            // a buffer of its length, which the call fills.
            let n = unsafe {
                libc::getattrlistbulk(
                    fd.as_raw_fd(),
                    (&raw mut wanted).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                )
            };
            let Ok(n) = usize::try_from(n) else {
                return Err(std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO));
            };
            if n == 0 {
                return Ok(());
            }
            let mut at = 0;
            for _ in 0..n {
                let len = buf
                    .get(at..at + 4)
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_ne_bytes)
                    .and_then(|l| usize::try_from(l).ok())
                    .filter(|&l| l >= 4)
                    .ok_or(libc::EIO)?;
                let entry = buf.get(at..at + len).ok_or(libc::EIO)?;
                match parse(entry, &wanted).ok_or(libc::EIO)? {
                    Ok(listed) => out.push(listed),
                    Err(name) => unparsed.push(name),
                }
                at += len;
            }
        }
    })?;
    for name in unparsed {
        if matches!(name.as_slice(), b"." | b"..") {
            continue;
        }
        let Ok(c) = std::ffi::CString::new(name.clone()) else {
            continue;
        };
        match rustix::fs::statat(fd, &c, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => out.push(Listed {
                name,
                kind: kind_of_mode(st.st_mode),
                id: FileId {
                    dev: dev(st.st_dev),
                    ino: st.st_ino,
                },
                attrs: attrs.then(|| attrs_of(&st)),
                mount_point: false,
            }),
            // Gone since the listing, which moved the folder's change
            // time: the next check reads it again.
            Err(Errno::NOENT) => {}
            Err(e) => return Err(e.raw_os_error()),
        }
    }
    Ok(out)
}

pub(crate) fn read(
    root: &Root,
    rel: &Path,
    opts: &Options,
    _: &mut Watch,
    _: Option<Reuse<'_>>,
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
    let stat = match rustix::fs::fstat(&fd) {
        Ok(st) => folder_stat(&st),
        Err(e) => return not_covered(None, Why::Failed(e.raw_os_error())),
    };
    if root.exclusions.holds(Some(stat.id), &root.path.join(rel)) {
        return not_covered(Some(stat), Why::Excluded);
    }
    if stat.mount != root.stat.mount {
        return not_covered(Some(stat), Why::Mount);
    }
    // Every entry's attributes with its name: none stat-ed, and so none
    // reused from what the index held (`Reuse` spares Linux a stat each).
    // A listing that fails part-way gives its error alone.
    let mut entries = match entries_of(fd.as_fd(), opts.attrs) {
        Ok(entries) => entries,
        Err(errno) => return not_covered(Some(stat), Why::CutShort(errno)),
    };
    if entries.iter().any(|e| e.name == b".private") {
        return not_covered(Some(stat), Why::Private);
    }
    // Read within a tick of its last change, or with a change time ahead
    // of the clock: a second change may have left that time as it was.
    let again = read_at < stat.ctime.plus(opts.tick);
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let mut listing = Listing::new(stat, read_at, again);
    for e in entries {
        let mount = e.mount_point
            || (!root.below.is_empty()
                && root
                    .mount_at(&rel.join(OsStr::from_bytes(&e.name)))
                    .is_some());
        let entry = if mount {
            // A mount point: what its listing gave is its volume's, and a
            // mount's are not this folder's.
            Entry {
                kind: Kind::Folder,
                id: FileId {
                    dev: stat.id.dev,
                    ino: e.id.ino,
                },
                attrs: None,
            }
        } else {
            Entry {
                kind: e.kind,
                id: e.id,
                attrs: e.attrs,
            }
        };
        listing.push(&e.name, entry);
    }
    (Some(stat), Outcome::Listed(listing))
}

pub(crate) fn verdict(
    root: &Root,
    points: &[(PathBuf, bool)],
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
        Ok(st) if kind_of_mode(st.st_mode) != Kind::Folder => Some(Stale::Replaced),
        Ok(st) => {
            let now = folder_stat(&st);
            if now.mount != root.stat.mount {
                Some(Stale::Mount)
            } else if now.id != stored.id {
                Some(Stale::Replaced)
            } else if now.ctime != stored.ctime {
                Some(Stale::Changed)
            } else {
                None
            }
        }
    }
}

/// [`verdict`]: no folder is watched for itself on macOS. Its events come
/// for a whole root (phase 5, its step 3).
pub(crate) fn verdict_watched(
    root: &Root,
    points: &[(PathBuf, bool)],
    rel: &Path,
    stored: StoredFolder,
    _: &Options,
) -> (Option<Stale>, Watch) {
    (verdict(root, points, rel, stored), Watch::Unasked)
}

/// The stat of the folder at `rel` by its path, no link followed anywhere
/// in it (`AT_SYMLINK_NOFOLLOW_ANY`): through its parent's handle where
/// the path is longer than the system resolves at once.
fn stat_path(root: &Root, rel: &Path) -> rustix::io::Result<Stat> {
    let path = root.path.join(rel);
    if path.as_os_str().len() < PATH_MAX {
        return rustix::fs::statat(
            rustix::fs::CWD,
            &path,
            AtFlags::from_bits_retain(SYMLINK_NOFOLLOW_ANY),
        );
    }
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = rel.file_name().unwrap_or_default();
    root.dir()
        .and_then(|d| open_beneath(&d, parent))
        .and_then(|fd| rustix::fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW))
}

/// A name's bytes as a path component, as the listing gave them.
pub(crate) use crate::mounts::component;

/// `path`'s bytes, put at the end of `buf`: as the system names it.
pub(crate) fn push_path(buf: &mut Vec<u8>, path: &Path) {
    buf.extend_from_slice(path.as_os_str().as_bytes());
}

/// The path `bytes` name, as [`push_path`] put them.
pub(crate) fn path_of(bytes: &[u8]) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Borrowed(Path::new(OsStr::from_bytes(bytes)))
}

#[cfg(test)]
mod tests;
