//! fanotify on Linux (the module above says what holds): the group, its
//! marks, its events read, and handles opened.
#![allow(unsafe_code)] // fanotify and file handles have no safe binding; each block says why it is sound.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rustix::fd::AsFd;
use rustix::fs::{AtFlags, Mode, OFlags, StatxFlags};

use super::{Folder, Heard, Marked, Refused, Resolved, Through};
use crate::FileId;
use crate::mounts::{Mount, Table};

/// How a folder is opened to mark its filesystem, and to open the handles
/// of folders on it (`open_by_handle_at` refuses an `O_PATH` descriptor).
const FOLDER: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// The most bytes a file handle holds (`MAX_HANDLE_SZ`).
const MOST_HANDLE: usize = 128;

/// What a filesystem is marked for: every change to a name, and to what
/// it names, folders included.
const MASK: u64 = libc::FAN_CREATE
    | libc::FAN_DELETE
    | libc::FAN_MOVED_FROM
    | libc::FAN_MOVED_TO
    | libc::FAN_MODIFY
    | libc::FAN_ATTRIB
    | libc::FAN_ONDIR;

/// A filesystem marked: its device and its events' id.
struct Mark {
    dev: (u32, u32),
    fsid: u64,
}

struct State {
    marks: Vec<Mark>,
    /// The folder cookies are made in.
    cookies: Option<Folder>,
}

/// A fanotify group and the filesystems it marks.
pub struct Group {
    fd: OwnedFd,
    generation: u64,
    state: Mutex<State>,
    buffer: Mutex<Box<[u8]>>,
}

impl std::fmt::Debug for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Group")
            .field("generation", &self.generation)
            .field("marks", &self.lock().marks.len())
            .finish_non_exhaustive()
    }
}

impl Group {
    /// A group of generation `generation`, which reports names with the
    /// folder they are in, and marks nothing yet.
    pub fn new(generation: u64) -> std::io::Result<Group> {
        // SAFETY: fanotify_init takes two words of flags and returns a new
        // descriptor or -1; no memory is shared with the kernel.
        let raw = unsafe {
            libc::fanotify_init(
                libc::FAN_CLASS_NOTIF
                    | libc::FAN_CLOEXEC
                    | libc::FAN_NONBLOCK
                    | libc::FAN_REPORT_DFID_NAME,
                (libc::O_RDONLY | libc::O_LARGEFILE) as u32,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Group {
            // SAFETY: `raw` was just returned by fanotify_init and nothing
            // else owns it.
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            generation,
            state: Mutex::new(State {
                marks: Vec::new(),
                cookies: None,
            }),
            buffer: Mutex::new(vec![0; 64 << 10].into_boxed_slice()),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Its handle, for a caller to wait on until it can be read.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mark the whole filesystem holding `folder` (as the system resolves
    /// it), which `table`, the mount table now, says it is on.
    pub fn mark(&self, folder: &Path, table: &Table) -> Result<Marked, Refused> {
        let mount = table.holding(folder).ok_or(Refused::NoMount)?;
        let mut state = self.lock();
        if let Some(m) = state.marks.iter().find(|m| m.dev == mount.dev) {
            return Ok(Marked {
                dev: m.dev,
                fsid: m.fsid,
                through: Through::Itself,
                new: false,
            });
        }
        let on = rustix::fs::open(folder, FOLDER, Mode::empty())
            .map_err(|e| Refused::Failed(e.raw_os_error()))?;
        let (on, through) = match mark(&self.fd, on.as_fd()) {
            Ok(()) => (on, Through::Itself),
            Err(e) if e == libc::EXDEV && mount.fstype == "btrfs" => {
                // The mount and its top are let go of once marked: the mark
                // is the superblock's.
                let (_mount, top) = top_level(mount).map_err(Refused::TopLevel)?;
                mark(&self.fd, top.as_fd()).map_err(refused)?;
                (top, Through::TopLevel)
            }
            Err(e) => return Err(refused(e)),
        };
        let fsid = fsid_of(&on).map_err(|e| {
            unmark(&self.fd, on.as_fd());
            Refused::Failed(e.raw_os_error())
        })?;
        if state.marks.iter().any(|m| m.fsid == fsid) {
            unmark(&self.fd, on.as_fd());
            return Err(Refused::SameId);
        }
        state.marks.push(Mark {
            dev: mount.dev,
            fsid,
        });
        Ok(Marked {
            dev: mount.dev,
            fsid,
            through,
            new: true,
        })
    }

    /// The filesystems marked, by device and events' id.
    pub fn marks(&self) -> Vec<((u32, u32), u64)> {
        self.lock().marks.iter().map(|m| (m.dev, m.fsid)).collect()
    }

    /// Forget the mark of the filesystem of this device, once the mount
    /// table holds it no longer: its superblock's end took the mark.
    pub fn forget(&self, dev: (u32, u32)) {
        self.lock().marks.retain(|m| m.dev != dev);
    }

    /// Hear cookies in `dir`, a folder of the caller's own on a filesystem
    /// marked: a file named `cookie-N` made in it is heard as
    /// [`Heard::Cookie`], after every event queued before it.
    pub fn watch_cookies(&self, dir: &Path, table: &Table) -> std::io::Result<()> {
        let mount = table.holding(dir).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no mount holds the folder")
        })?;
        let mut state = self.lock();
        let Some(mark) = state.marks.iter().find(|m| m.dev == mount.dev) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the cookies' folder is on a filesystem not marked",
            ));
        };
        let fsid = mark.fsid;
        let fd = rustix::fs::open(dir, FOLDER, Mode::empty())?;
        let handle = handle_of_folder(fd.as_fd())?;
        state.cookies = Some(Folder { fsid, handle });
        Ok(())
    }

    /// Everything heard and not read yet, added to `heard`, without
    /// waiting: [`Group::fd`] says when there is more.
    pub fn read(&self, heard: &mut Vec<Heard>) -> std::io::Result<()> {
        let mut buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        let cookies = self.lock().cookies.clone();
        loop {
            let n = match rustix::io::read(&self.fd, &mut buffer[..]) {
                Ok(n) => n,
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            };
            events(&buffer[..n], cookies.as_ref(), heard)?;
        }
    }

    /// A resolver of this group's handles, with the mount table now.
    pub fn resolver<'a>(&'a self, table: &'a Table) -> Resolver<'a> {
        Resolver {
            group: self,
            table,
            opened: HashMap::new(),
        }
    }
}

/// Opens folders' handles, through a mount of their filesystem it opens
/// once and keeps while it lives: one for each batch of events.
pub struct Resolver<'a> {
    group: &'a Group,
    table: &'a Table,
    /// A mount's folder opened, by events' id, or why none could be.
    opened: HashMap<u64, Result<OwnedFd, i32>>,
}

impl Resolver<'_> {
    /// The folder `folder`'s handle opens to.
    pub fn resolve(&mut self, folder: &Folder) -> Resolved {
        let dev = {
            let state = self.group.lock();
            match state.marks.iter().find(|m| m.fsid == folder.fsid) {
                Some(m) => m.dev,
                None => return Resolved::Unmarked,
            }
        };
        let table = self.table;
        let on = self
            .opened
            .entry(folder.fsid)
            .or_insert_with(|| open_mount_of(table, dev));
        let on = match on.as_ref() {
            Ok(fd) => fd.as_fd(),
            Err(e) => return Resolved::Failed(*e),
        };
        match open_handle(on, &folder.handle) {
            Ok(fd) => match rustix::fs::fstat(&fd) {
                Ok(st) => Resolved::Is(FileId {
                    dev: st.st_dev,
                    ino: st.st_ino,
                }),
                Err(e) => Resolved::Failed(e.raw_os_error()),
            },
            Err(e) if e == libc::ESTALE => Resolved::Gone,
            Err(e) => Resolved::Failed(e),
        }
    }
}

/// A folder of a mount of the filesystem of device `dev`, as the mount
/// table names it, opened and known to be that mount by its id: a mount
/// put over its point since the table was read is not taken for it.
fn open_mount_of(table: &Table, dev: (u32, u32)) -> Result<OwnedFd, i32> {
    let mut last = libc::ENOENT;
    // The topmost mount at a point is the later in the table.
    for mount in table.mounts().iter().rev().filter(|m| m.dev == dev) {
        let Ok(fd) = rustix::fs::open(&mount.point, FOLDER, Mode::empty()) else {
            continue;
        };
        match rustix::fs::statx(&fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID) {
            Ok(st) if st.stx_mnt_id == mount.id => return Ok(fd),
            Ok(_) => last = libc::EXDEV,
            Err(e) => last = e.raw_os_error(),
        }
    }
    Err(last)
}

/// Mark the whole filesystem of the open folder `on`.
fn mark(group: &OwnedFd, on: BorrowedFd<'_>) -> Result<(), i32> {
    // SAFETY: `group` is a fanotify descriptor and `on` an open folder; the
    // path is a NUL-terminated literal the call only reads.
    let done = unsafe {
        libc::fanotify_mark(
            group.as_raw_fd(),
            libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
            MASK,
            on.as_raw_fd(),
            c".".as_ptr(),
        )
    };
    if done == 0 { Ok(()) } else { Err(errno()) }
}

/// Take back the mark of the open folder `on`'s filesystem, made just
/// before and not kept.
fn unmark(group: &OwnedFd, on: BorrowedFd<'_>) {
    // SAFETY: as in `mark`. A failure leaves a mark whose events have an
    // id no mark kept has, which resolve as unmarked.
    unsafe {
        libc::fanotify_mark(
            group.as_raw_fd(),
            libc::FAN_MARK_REMOVE | libc::FAN_MARK_FILESYSTEM,
            MASK,
            on.as_raw_fd(),
            c".".as_ptr(),
        );
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn refused(e: i32) -> Refused {
    match e {
        libc::EPERM => Refused::NotPermitted,
        libc::ENODEV | libc::EOPNOTSUPP => Refused::NoHandles,
        e => Refused::Failed(e),
    }
}

/// A filesystem's id as its events give it: `statfs`'s two words, the
/// first low.
fn fsid_of(on: &OwnedFd) -> Result<u64, rustix::io::Errno> {
    let st = rustix::fs::fstatfs(on)?;
    // SAFETY: `__fsid_t` is two plain words, read as such.
    let [low, high]: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
    Ok(words(low, high))
}

fn words(low: i32, high: i32) -> u64 {
    u64::from(low as u32) | (u64::from(high as u32) << 32)
}

/// The btrfs filesystem of `mount` mounted again at its top-level
/// subvolume (number 5), detached, and that mount's top opened.
///
/// The mount is attached to no folder, so no other process sees it, and it
/// goes when its descriptors close. It is read-only twice over: as a
/// mount, and as asked of btrfs, which leaves a writable filesystem as it
/// is when a subvolume is mounted read-only.
fn top_level(mount: &Mount) -> Result<(OwnedFd, OwnedFd), String> {
    use rustix::mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, fsconfig_create, fsconfig_set_flag,
        fsconfig_set_string, fsmount, fsopen,
    };
    let failed = |step: &'static str| move |e: rustix::io::Errno| format!("{step} failed ({e})");
    let fs = fsopen("btrfs", FsOpenFlags::FSOPEN_CLOEXEC).map_err(failed("fsopen"))?;
    fsconfig_set_string(&fs, "source", mount.source.as_os_str())
        .map_err(failed("naming the device"))?;
    fsconfig_set_string(&fs, "subvolid", "5").map_err(failed("naming the subvolume"))?;
    fsconfig_set_flag(&fs, "ro").map_err(failed("asking read-only"))?;
    fsconfig_create(&fs).map_err(failed("mounting"))?;
    let detached = fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::MOUNT_ATTR_RDONLY
            | MountAttrFlags::MOUNT_ATTR_NOSUID
            | MountAttrFlags::MOUNT_ATTR_NODEV
            | MountAttrFlags::MOUNT_ATTR_NOEXEC,
    )
    .map_err(failed("fsmount"))?;
    let top = rustix::fs::openat(&detached, ".", FOLDER, Mode::empty())
        .map_err(failed("opening its top"))?;
    Ok((detached, top))
}

/// A `struct file_handle` with room for the largest handle, aligned as
/// the kernel reads one.
#[repr(C)]
struct Handle {
    bytes: u32,
    kind: i32,
    data: [u8; MOST_HANDLE],
}

/// The open folder `dir`'s handle, as events give it: its length, its
/// type, then its bytes.
fn handle_of_folder(dir: BorrowedFd<'_>) -> std::io::Result<Box<[u8]>> {
    let mut h = Handle {
        bytes: MOST_HANDLE as u32,
        kind: 0,
        data: [0; MOST_HANDLE],
    };
    let mut mount_id = 0;
    // SAFETY: `h` is a `struct file_handle` with room for `bytes` bytes, as
    // its first word says; the path is an empty literal, so the call names
    // `dir` itself; `mount_id` is a word the call writes.
    let done = unsafe {
        libc::name_to_handle_at(
            dir.as_raw_fd(),
            c"".as_ptr(),
            (&raw mut h).cast(),
            &raw mut mount_id,
            libc::AT_EMPTY_PATH,
        )
    };
    if done != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = (h.bytes as usize).min(MOST_HANDLE);
    let mut out = Vec::with_capacity(8 + len);
    out.extend_from_slice(&h.bytes.to_ne_bytes());
    out.extend_from_slice(&h.kind.to_ne_bytes());
    out.extend_from_slice(&h.data[..len]);
    Ok(out.into_boxed_slice())
}

/// The folder of handle `handle` (as events give it), opened through
/// `on`, a folder of its filesystem.
fn open_handle(on: BorrowedFd<'_>, handle: &[u8]) -> Result<OwnedFd, i32> {
    let word = |at: usize| -> Option<[u8; 4]> { handle.get(at..at + 4)?.try_into().ok() };
    let (Some(bytes), Some(kind)) = (word(0), word(4)) else {
        return Err(libc::EINVAL);
    };
    let len = u32::from_ne_bytes(bytes) as usize;
    let Some(data) = handle.get(8..8 + len).filter(|_| len <= MOST_HANDLE) else {
        return Err(libc::EINVAL);
    };
    let mut h = Handle {
        bytes: len as u32,
        kind: i32::from_ne_bytes(kind),
        data: [0; MOST_HANDLE],
    };
    h.data[..len].copy_from_slice(data);
    // SAFETY: `h` holds a whole `struct file_handle` (its length, its type,
    // then that many bytes), aligned for one, and the kernel only reads it.
    let raw = unsafe {
        libc::open_by_handle_at(
            on.as_raw_fd(),
            (&raw mut h).cast(),
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(errno());
    }
    // SAFETY: `raw` was just returned by open_by_handle_at and nothing else
    // owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// The events a read gave, added to `heard`.
fn events(read: &[u8], cookies: Option<&Folder>, heard: &mut Vec<Heard>) -> std::io::Result<()> {
    let meta_len = size_of::<libc::fanotify_event_metadata>();
    let mut at = 0;
    while at + meta_len <= read.len() {
        // SAFETY: `meta_len` bytes from `at` are within what read wrote;
        // read_unaligned copies them whatever their alignment.
        let meta: libc::fanotify_event_metadata =
            unsafe { std::ptr::read_unaligned(read.as_ptr().add(at).cast()) };
        if meta.vers != libc::FANOTIFY_METADATA_VERSION {
            return Err(std::io::Error::other(format!(
                "fanotify metadata of version {}",
                meta.vers
            )));
        }
        let end = at + meta.event_len as usize;
        if (meta.event_len as usize) < meta_len || end > read.len() {
            return Err(std::io::Error::other("an event cut short"));
        }
        if meta.fd >= 0 {
            // SAFETY: an event's descriptor is the reader's to close; a
            // group that reports handles gives none, but one is closed if
            // given.
            drop(unsafe { OwnedFd::from_raw_fd(meta.fd) });
        }
        if meta.mask & libc::FAN_Q_OVERFLOW != 0 {
            heard.push(Heard::Overflow);
        } else if let Some((folder, name)) = named(&read[at..end], meta.metadata_len as usize) {
            heard.push(said(folder, name, meta.mask, cookies));
        }
        at = end;
    }
    Ok(())
}

/// What one event names: its folder, and the name in it.
fn named(event: &[u8], from: usize) -> Option<(Folder, &[u8])> {
    let header_len = size_of::<libc::fanotify_event_info_header>();
    let fid_len = size_of::<libc::fanotify_event_info_fid>();
    let mut info = from;
    while info + header_len <= event.len() {
        // SAFETY: `header_len` bytes from `info` are within the event.
        let header: libc::fanotify_event_info_header =
            unsafe { std::ptr::read_unaligned(event.as_ptr().add(info).cast()) };
        if header.len == 0 {
            return None;
        }
        let end = (info + header.len as usize).min(event.len());
        let with_name = header.info_type == libc::FAN_EVENT_INFO_TYPE_DFID_NAME;
        let folder = with_name || header.info_type == libc::FAN_EVENT_INFO_TYPE_DFID;
        if folder && info + fid_len <= end {
            // SAFETY: `fid_len` bytes from `info` are within the record.
            let info_fid: libc::fanotify_event_info_fid =
                unsafe { std::ptr::read_unaligned(event.as_ptr().add(info).cast()) };
            let record = &event[info + fid_len..end];
            let handle = handle_in(record)?;
            let name = if with_name {
                let rest = &record[handle.len()..];
                &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())]
            } else {
                b"."
            };
            let fsid = words(info_fid.fsid.val[0], info_fid.fsid.val[1]);
            return Some((
                Folder {
                    fsid,
                    handle: handle.into(),
                },
                name,
            ));
        }
        info = end;
    }
    None
}

/// The handle a record holds, as a `struct file_handle`: its length (4
/// bytes), its type (4), then its bytes.
fn handle_in(record: &[u8]) -> Option<&[u8]> {
    let length = u32::from_ne_bytes(record.get(..4)?.try_into().ok()?) as usize;
    record.get(..8 + length)
}

/// What an event in `folder`, of `name`, is heard as.
fn said(folder: Folder, name: &[u8], mask: u64, cookies: Option<&Folder>) -> Heard {
    if cookies == Some(&folder) {
        if mask & libc::FAN_CREATE != 0
            && let Some(n) = cookie_number(name)
        {
            return Heard::Cookie(n);
        }
        // The cookies' folder is the engine's own: nothing else in it
        // matters, and it is never indexed. Heard as the folder changed.
        return Heard::Changed(folder);
    }
    if name == b"." {
        Heard::Changed(folder)
    } else {
        Heard::Entry(folder, name.into())
    }
}

/// The number of a cookie's file, `cookie-N`.
fn cookie_number(name: &[u8]) -> Option<u64> {
    std::str::from_utf8(name.strip_prefix(b"cookie-")?)
        .ok()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(handle: &[u8], name: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&(handle.len() as u32).to_ne_bytes());
        r.extend_from_slice(&1i32.to_ne_bytes());
        r.extend_from_slice(handle);
        r.extend_from_slice(name);
        r.push(0);
        r
    }

    /// A record's handle is its length and type, then its bytes; the name
    /// follows, up to its NUL.
    #[test]
    fn a_record_holds_a_handle_then_a_name() {
        let r = record(&[9, 9, 9], b"a b.txt");
        assert_eq!(handle_in(&r), Some(&r[..11]));
        assert_eq!(handle_in(&r[..10]), None, "a handle cut short");
    }

    /// A cookie is heard in its folder alone, made; a name is an entry, and
    /// `.` the folder itself.
    #[test]
    fn a_cookie_is_heard_in_its_folder_alone() {
        let cookies = Folder {
            fsid: 7,
            handle: [1, 2, 3].into(),
        };
        let other = Folder {
            fsid: 7,
            handle: [4, 5, 6].into(),
        };
        assert_eq!(
            said(
                cookies.clone(),
                b"cookie-12",
                libc::FAN_CREATE,
                Some(&cookies)
            ),
            Heard::Cookie(12)
        );
        assert_eq!(
            said(
                cookies.clone(),
                b"cookie-12",
                libc::FAN_DELETE,
                Some(&cookies)
            ),
            Heard::Changed(cookies.clone())
        );
        assert_eq!(
            said(
                other.clone(),
                b"cookie-12",
                libc::FAN_CREATE,
                Some(&cookies)
            ),
            Heard::Entry(other.clone(), b"cookie-12"[..].into())
        );
        assert_eq!(
            said(other.clone(), b".", libc::FAN_ATTRIB, None),
            Heard::Changed(other)
        );
    }
}
