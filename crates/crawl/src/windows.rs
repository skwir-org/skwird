//! The crawl and the check on Windows (`docs/PLAN.md`, phase 4, step 2).
//!
//! - **A folder is opened relative to the root's handle** (`NtCreateFile`
//!   with the root as its root directory), never through a reparse point
//!   (`OBJ_DONT_REPARSE`, `FILE_OPEN_REPARSE_POINT`): a link, a junction,
//!   a volume mounted in a folder is not crossed, as a link or a mount is
//!   not on Linux. A folder opened that is a reparse point is not read.
//! - **It is listed in batches through that handle** (`NtQueryDirectoryFile`,
//!   `FileIdFullDirectoryInformation`): each entry's name, kind, file id,
//!   size and times, with no open for each.
//! - **A file's identity** is its volume's serial and its file id (64
//!   bits: NTFS's; `ReFS`'s take 128, open question 7). A folder's mount is
//!   its volume's serial.
//! - **Names are UTF-16**, kept as their WTF-8 ([`crate::wtf8`]).
//! - **A path is named plainly** (`C:\Users\...`), as clients name it, and
//!   opened in its verbatim form (`\\?\C:\Users\...`), which takes any
//!   length.
//! - **Mounts are found where the listing meets them:** a folder that is
//!   a volume's mount point, or a reparse point of another kind (a filter
//!   driver's: cloud files, projected file systems), is not entered, and
//!   is not covered (a mount). There is no mount table to read.
//!
//! What Wine answers (`tools/wine-tests`) is not NTFS: its files are the
//! host's, its links' reparse tags are blank, and it follows a link of
//! the host's even when asked not to. Its tests prove this code against
//! Wine's answers.

#![allow(unsafe_code)] // Windows' calls have no safe binding; each block says why it is sound.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_ID_FULL_DIR_INFORMATION, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileIdFullDirectoryInformation,
    NtCreateFile, NtQueryDirectoryFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
    STATUS_NAME_TOO_LONG, STATUS_NO_MORE_FILES, STATUS_UNSUCCESSFUL, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileBasicInfo, FileIdInfo, GetDriveTypeW, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, GetVolumeInformationByHandleW, OPEN_EXISTING, SYNCHRONIZE,
    VOLUME_NAME_NONE,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};

use crate::mounts::Table;
use crate::walk::{Reuse, Stale, StoredFolder, mount_why};
use crate::{
    Attrs, Entry, Error, Exclusions, FileId, FolderStat, Kind, Listing, Options, Outcome, Time,
    Watch, Why,
};

/// A drive that is a network share (`GetDriveTypeW`).
const DRIVE_REMOTE: u32 = 4;
/// The reparse tags of what is a link: a symbolic link, a junction or a
/// volume's mount point (told apart by their target), a link made by
/// Linux's subsystem, an app's alias.
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_LX_SYMLINK: u32 = 0xA000_001D;
const TAG_APPEXECLINK: u32 = 0x8000_001B;
/// Asks a reparse point for its data (`winioctl.h`).
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
/// The longest name an open takes, in UTF-16 units, well below the
/// 32,767 a `UNICODE_STRING` holds: a longer one is opened a part at a
/// time.
const PART: usize = 30_000;

/// Why an open or a stat failed, as the crawl and the check tell it.
#[derive(Clone, Copy, Debug)]
enum Fail {
    Gone,
    Denied,
    /// A link, a file or another folder where the folder was.
    Replaced,
    /// Any other failure: an `NTSTATUS`, or a Win32 error.
    Other(i32),
}

impl Fail {
    fn of_status(status: NTSTATUS) -> Fail {
        // The status's bits, as `ntstatus.h` writes them.
        #[allow(clippy::cast_sign_loss)]
        match status as u32 {
            // Not found: the name, a folder above it, or deleted, pending.
            0xC000_0034 | 0xC000_003A | 0xC000_0056 => Fail::Gone,
            0xC000_0022 => Fail::Denied,
            // Not a folder; a reparse point met; a link stopped at.
            0xC000_0103 | 0xC000_050B | 0x8000_002D => Fail::Replaced,
            _ => Fail::Other(status),
        }
    }

    fn of_error(error: u32) -> Fail {
        match error {
            // Its file, or a folder of its path, not found.
            2 | 3 => Fail::Gone,
            5 => Fail::Denied,
            // Not a folder.
            267 => Fail::Replaced,
            e => Fail::Other(i32::try_from(e).unwrap_or(i32::MAX)),
        }
    }

    fn number(self) -> i32 {
        match self {
            Fail::Other(n) => n,
            // Their Win32 errors.
            Fail::Gone => 2,
            Fail::Denied => 5,
            Fail::Replaced => 267,
        }
    }
}

/// A handle, closed once dropped.
#[derive(Debug)]
struct Handle(HANDLE);

// SAFETY: a handle names a kernel object, usable from any thread; it is
// closed once, when dropped, and never used after.
unsafe impl Send for Handle {}
// SAFETY: as above; the calls made through a shared handle (queries of
// its information, opens relative to it) do not change it.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this process, and is closed here
        // once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A root, opened: the folder a crawl reads below and a check proves.
///
/// Its folder is held open only while it is read ([`Root::let_go`]
/// after), as on Linux: opened again by its path when next read, and
/// taken only if it is the same folder, on the same volume.
pub struct Root {
    path: PathBuf,
    handle: RwLock<Option<Arc<Handle>>>,
    stat: FolderStat,
    fs: String,
    table: Table,
    /// Where volumes, or filter drivers' folders, start below it, found
    /// by listing their parents: none reaches a network.
    below: Mutex<HashMap<PathBuf, bool>>,
    /// Its path within its volume, links resolved: what every folder
    /// opened below it is held to ([`Root::open_folder`]).
    inside: Vec<u16>,
    exclusions: Exclusions,
}

impl Root {
    /// The folder at `path`, links in its path followed.
    pub fn open(path: &Path, exclusions: Exclusions) -> Result<Root, Error> {
        Root::open_with(path, exclusions, Table::parse(b""))
    }

    /// The folder at `path`. Windows has no mount table: `table` is kept,
    /// and says nothing.
    pub fn open_with(path: &Path, exclusions: Exclusions, table: Table) -> Result<Root, Error> {
        // Told from its spelling first, so that a root on a dead server is
        // never opened.
        if on_network(path) {
            return Err(Error::Network);
        }
        let path = crate::canonical(path).map_err(Error::Root)?;
        if on_network(&path) {
            return Err(Error::Network);
        }
        let handle = open_path(&path).map_err(Error::Root)?;
        let (stat, attributes) = stat_of(&handle).map_err(Error::Root)?;
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(Error::Root(std::io::Error::from_raw_os_error(267)));
        }
        let fs = filesystem(&handle);
        let inside =
            final_path(&handle).ok_or_else(|| Error::Root(std::io::Error::last_os_error()))?;
        Ok(Root {
            path,
            handle: RwLock::new(Some(Arc::new(handle))),
            stat,
            fs,
            table,
            below: Mutex::new(HashMap::new()),
            inside,
            exclusions,
        })
    }

    /// Its folder, open: as it was opened, or opened again by its path if
    /// let go of, refused if that is no longer the folder it was.
    fn dir(&self) -> Result<Arc<Handle>, Fail> {
        if let Some(handle) = self
            .handle
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            return Ok(Arc::clone(handle));
        }
        let mut held = self.handle.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = held.as_ref() {
            return Ok(Arc::clone(handle));
        }
        let handle = open_path(&self.path).map_err(|e| {
            Fail::of_error(
                e.raw_os_error()
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
            )
        })?;
        let (stat, _) = stat_of(&handle).map_err(|_| Fail::Other(0))?;
        if stat.id != self.stat.id || stat.mount != self.stat.mount {
            return Err(Fail::Replaced);
        }
        let handle = Arc::new(handle);
        *held = Some(Arc::clone(&handle));
        Ok(handle)
    }

    /// The folder at `rel` below it, opened through no reparse point. The
    /// system refuses to cross one (`OBJ_DONT_REPARSE`), and the folder
    /// opened must be the one named: its path within its volume, links
    /// resolved, is the root's and `rel`'s. Wine crosses a link in a
    /// path's middle whatever it is asked; on Windows, this is a second
    /// guard, for one more call a folder.
    fn open_folder(&self, rel: &Path) -> Result<Handle, Fail> {
        let handle = open_beneath(&*self.dir()?, rel)?;
        let mut expected = self.inside.clone();
        for part in rel.components() {
            if let Component::Normal(name) = part {
                if expected.last() != Some(&u16::from(b'\\')) {
                    expected.push(u16::from(b'\\'));
                }
                expected.extend(name.encode_wide());
            }
        }
        match final_path(&handle) {
            Some(found) if found == expected => Ok(handle),
            Some(_) => Err(Fail::Replaced),
            None => Err(Fail::Other(
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            )),
        }
    }

    /// No folder's ACL is read on Windows: its security descriptor is the
    /// system service's, which checks while impersonating (phase 4, step
    /// 7).
    pub fn access_acl(&self, _: &Path, _: FileId) -> Option<crate::acl::Acl> {
        None
    }

    /// Let go of its folder until it is next read.
    pub fn let_go(&self) {
        *self.handle.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Its path, named plainly.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stat(&self) -> &FolderStat {
        &self.stat
    }

    /// Its filesystem's name (`NTFS`, `ReFS`).
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

    /// Where mounts start below it, by path below it, as its listings
    /// found them.
    pub fn mounts_below(&self) -> Vec<PathBuf> {
        self.below
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Nothing to read: mounts are found by listing their parents.
    // The same call as on Linux, which reads its table and may fail.
    #[allow(clippy::unnecessary_wraps)]
    pub fn reread_mounts(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    pub fn set_table(&mut self, table: Table) {
        self.table = table;
    }

    pub(crate) fn mount_at(&self, rel: &Path) -> Option<bool> {
        self.below
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(rel)
            .copied()
    }

    pub(crate) fn points(&self) -> Vec<(PathBuf, bool)> {
        self.below
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(p, &n)| (p.clone(), n))
            .collect()
    }

    /// A mount found at `rel`, listing its parent.
    fn found_mount(&self, rel: PathBuf) {
        self.below
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(rel, false);
    }

    /// Whether `rel` or a folder above it, below the root, is excluded by
    /// its path.
    fn excluded_by_path(&self, rel: &Path) -> bool {
        rel.ancestors()
            .filter(|a| !a.as_os_str().is_empty())
            .any(|a| self.exclusions.holds(None, &self.path.join(a)))
    }
}

/// Whether `path` is on a network share: a UNC path, or a drive that is
/// one.
fn on_network(path: &Path) -> bool {
    match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => true,
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                let root: Vec<u16> = [u16::from(d), u16::from(b':'), u16::from(b'\\'), 0].into();
                // SAFETY: a NUL-terminated name, alive for the call.
                unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
            }
            _ => false,
        },
        _ => false,
    }
}

/// The identity of the folder at `path`, if a folder is there.
pub(crate) fn folder_id(path: &Path) -> Option<FileId> {
    let handle = open_path(path).ok()?;
    let (stat, attributes) = stat_of(&handle).ok()?;
    (attributes & FILE_ATTRIBUTE_DIRECTORY != 0).then_some(stat.id)
}

/// `path` named plainly: `C:\Users` for `\\?\C:\Users`, as clients name
/// it. A verbatim path of another kind is kept as it is.
pub(crate) fn plain(path: &Path) -> PathBuf {
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::VerbatimDisk(d) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(d)));
                out.extend(parts.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// `path` in its verbatim form, which takes any length, NUL-terminated.
fn verbatim(path: &Path) -> Vec<u16> {
    let mut parts = path.components();
    let start = match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) => format!(r"\\?\{}:", char::from(d)),
            Prefix::UNC(server, share) => format!(
                r"\\?\UNC\{}\{}",
                server.to_string_lossy(),
                share.to_string_lossy()
            ),
            // Verbatim already, or a device's: as it is.
            _ => return path.as_os_str().encode_wide().chain([0]).collect(),
        },
        _ => return path.as_os_str().encode_wide().chain([0]).collect(),
    };
    // Rebuilt from its components: a verbatim path takes `\` alone as a
    // separator, and no `.` or `..`.
    let mut out: Vec<u16> = start.encode_utf16().collect();
    for part in parts {
        if let Component::Normal(name) = part {
            out.push(u16::from(b'\\'));
            out.extend(name.encode_wide());
        }
    }
    if out.last() == Some(&u16::from(b':')) {
        out.push(u16::from(b'\\'));
    }
    out.push(0);
    out
}

/// The folder at `path`, opened itself, never what it links to.
fn open_path(path: &Path) -> std::io::Result<Handle> {
    let name = verbatim(path);
    // SAFETY: a NUL-terminated name alive for the call, no security
    // attributes, no template; the handle given is ours to close.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Handle(handle))
}

/// The folder at `rel` below the folder open as `dir`, opened by a name
/// that crosses no reparse point, a part at a time where it is longer
/// than an open takes.
fn open_beneath(dir: &Handle, rel: &Path) -> Result<Handle, Fail> {
    let mut parts: Vec<Vec<u16>> = vec![Vec::new()];
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(Fail::Replaced);
        };
        let name: Vec<u16> = name.encode_wide().collect();
        let part = parts.last_mut().expect("one part at least");
        if !part.is_empty() && part.len() + 1 + name.len() > PART {
            parts.push(name);
        } else {
            if !part.is_empty() {
                part.push(u16::from(b'\\'));
            }
            part.extend(name);
        }
    }
    let mut parts = parts.into_iter();
    let mut handle = nt_open(dir.0, &parts.next().unwrap_or_default())?;
    for part in parts {
        handle = nt_open(handle.0, &part)?;
    }
    Ok(handle)
}

/// `name` below the folder open as `dir`, opened as a folder, itself if
/// it is a reparse point, and through none above it. An empty name is the
/// folder itself, opened again.
fn nt_open(dir: HANDLE, name: &[u16]) -> Result<Handle, Fail> {
    let bytes = u16::try_from(name.len() * 2).map_err(|_| Fail::Other(STATUS_NAME_TOO_LONG))?;
    let unicode = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(u32::MAX),
        RootDirectory: dir,
        ObjectName: &raw const unicode,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: an all-zero status block is a valid one to be written.
    let mut io: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer is to a value alive for the call; the name
    // is not written; the handle given is ours to close.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &raw const attributes,
            &raw mut io,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_REPARSE_POINT
                | FILE_OPEN_FOR_BACKUP_INTENT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(Fail::of_status(status));
    }
    Ok(Handle(handle))
}

/// One kind of a handle's information.
fn info<T>(handle: &Handle, class: i32) -> std::io::Result<T> {
    let mut out = std::mem::MaybeUninit::<T>::zeroed();
    // SAFETY: the buffer is `T`'s size, the size given; `T` is the
    // structure `class` writes, made of integers, valid whatever its bytes.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            class,
            out.as_mut_ptr().cast::<c_void>(),
            u32::try_from(size_of::<T>()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: zeroed, then written by the call: initialized either way.
    Ok(unsafe { out.assume_init() })
}

/// A folder's own facts, through its handle, and its attributes.
fn stat_of(handle: &Handle) -> std::io::Result<(FolderStat, u32)> {
    let id: FILE_ID_INFO = info(handle, FileIdInfo)?;
    let basic: FILE_BASIC_INFO = info(handle, FileBasicInfo)?;
    let stat = FolderStat {
        id: FileId {
            dev: id.VolumeSerialNumber,
            ino: low_bits(&id.FileId.Identifier),
        },
        mount: id.VolumeSerialNumber,
        ctime: time(basic.ChangeTime),
        mtime: time(basic.LastWriteTime),
        // No owner, group or mode bits on Windows: its security
        // descriptor is the service's (phase 4, step 7).
        owner: 0,
        group: 0,
        mode: 0,
    };
    Ok((stat, basic.FileAttributes))
}

/// The 64 bits of a 128-bit file id that NTFS gives (its file reference).
fn low_bits(id: &[u8; 16]) -> u64 {
    let mut low = [0; 8];
    low.copy_from_slice(&id[..8]);
    u64::from_le_bytes(low)
}

/// A file time (100 ns since 1601) as an instant.
fn time(filetime: i64) -> Time {
    // 1601 to 1970, in 100 ns.
    const EPOCH: i64 = 116_444_736_000_000_000;
    let since = filetime.saturating_sub(EPOCH);
    Time {
        sec: since.div_euclid(10_000_000),
        // Below 10,000,000 × 100.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        nsec: (since.rem_euclid(10_000_000) * 100) as u32,
    }
}

/// The path of what `handle` opened within its volume, links resolved:
/// what it is, whatever name opened it.
fn final_path(handle: &Handle) -> Option<Vec<u16>> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: the buffer and its length in units; the count returned
        // says how much was written, or how much is needed.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                handle.0,
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                FILE_NAME_NORMALIZED | VOLUME_NAME_NONE,
            )
        } as usize;
        if written == 0 {
            return None;
        }
        if written < buffer.len() {
            buffer.truncate(written);
            return Some(buffer);
        }
        buffer.resize(written + 1, 0);
    }
}

/// The filesystem's name, from its volume.
fn filesystem(handle: &Handle) -> String {
    let mut name = [0u16; 64];
    // SAFETY: the buffer and its length in units; no other field asked.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle.0,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            64,
        )
    };
    if ok == 0 {
        return "unknown".into();
    }
    let end = name.iter().position(|&u| u == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

/// An entry as a listing gave it.
struct Item {
    /// Its WTF-8.
    name: Vec<u8>,
    attributes: u32,
    /// Its reparse tag, if it is a reparse point: none on Wine.
    tag: u32,
    id: u64,
    size: u64,
    alloc: u64,
    ctime: Time,
    mtime: Time,
}

/// The entries of the folder open as `dir`, `.` and `..` left out. A
/// listing that fails part-way gives its error alone.
fn list(dir: &Handle) -> Result<Vec<Item>, Fail> {
    let mut buffer = vec![0u64; 8 * 1024];
    let mut items = Vec::new();
    let mut first = true;
    loop {
        // SAFETY: an all-zero status block is a valid one to be written.
        let mut io: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer and its length in bytes, aligned for the
        // records it is given; no event, no routine, no name to match.
        let status = unsafe {
            NtQueryDirectoryFile(
                dir.0,
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &raw mut io,
                buffer.as_mut_ptr().cast::<c_void>(),
                u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX),
                FileIdFullDirectoryInformation,
                false,
                std::ptr::null(),
                first,
            )
        };
        first = false;
        if status == STATUS_NO_MORE_FILES {
            return Ok(items);
        }
        if status < 0 {
            return Err(Fail::of_status(status));
        }
        let bytes: &[u8] = bytemuck(&buffer);
        let mut at = 0;
        loop {
            let Some(item) = record(bytes, at) else {
                return Err(Fail::Other(STATUS_UNSUCCESSFUL));
            };
            let (item, next) = item;
            if !matches!(item.name.as_slice(), b"." | b"..") {
                items.push(item);
            }
            if next == 0 {
                break;
            }
            at += next;
        }
    }
}

/// The buffer's bytes.
fn bytemuck(buffer: &[u64]) -> &[u8] {
    // SAFETY: a `u64` slice is as many bytes, each initialized, aligned
    // for a byte's reading.
    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) }
}

/// The record at `at` in a listing's buffer, and the offset of the next
/// one (0 for none): none if it would read past the buffer.
fn record(bytes: &[u8], at: usize) -> Option<(Item, usize)> {
    let head = std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileName);
    let fixed = bytes.get(at..at + head)?;
    let u32_at = |o: usize| u32::from_le_bytes(fixed[o..o + 4].try_into().expect("4 bytes"));
    let i64_at = |o: usize| i64::from_le_bytes(fixed[o..o + 8].try_into().expect("8 bytes"));
    let u64_at = |o: usize| u64::from_le_bytes(fixed[o..o + 8].try_into().expect("8 bytes"));
    let next = u32_at(std::mem::offset_of!(
        FILE_ID_FULL_DIR_INFORMATION,
        NextEntryOffset
    ));
    let name_bytes = u32_at(std::mem::offset_of!(
        FILE_ID_FULL_DIR_INFORMATION,
        FileNameLength
    ));
    let attributes = u32_at(std::mem::offset_of!(
        FILE_ID_FULL_DIR_INFORMATION,
        FileAttributes
    ));
    let name = bytes.get(at + head..at + head + name_bytes as usize)?;
    let wide = name
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]));
    let mut wtf8 = Vec::with_capacity(name.len());
    crate::wtf8::encode(wide, &mut wtf8);
    let reparse = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let field = |o| i64_at(o);
    let item = Item {
        name: wtf8,
        attributes,
        // A reparse point's tag is in the extended attributes' field.
        tag: if reparse {
            u32_at(std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, EaSize))
        } else {
            0
        },
        id: u64_at(std::mem::offset_of!(FILE_ID_FULL_DIR_INFORMATION, FileId)),
        size: u64_at(std::mem::offset_of!(
            FILE_ID_FULL_DIR_INFORMATION,
            EndOfFile
        )),
        alloc: u64_at(std::mem::offset_of!(
            FILE_ID_FULL_DIR_INFORMATION,
            AllocationSize
        )),
        ctime: time(field(std::mem::offset_of!(
            FILE_ID_FULL_DIR_INFORMATION,
            ChangeTime
        ))),
        mtime: time(field(std::mem::offset_of!(
            FILE_ID_FULL_DIR_INFORMATION,
            LastWriteTime
        ))),
    };
    Some((item, next as usize))
}

/// What an entry is, and whether it is a mount, not to be entered.
fn classify(dir: &Handle, item: &Item) -> (Kind, bool) {
    let folder = item.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if item.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return (if folder { Kind::Folder } else { Kind::File }, false);
    }
    match item.tag {
        // A junction, or a volume mounted there: its target tells.
        TAG_MOUNT_POINT if folder && mounts_a_volume(dir, &item.name) => (Kind::Folder, true),
        // A link of any kind; Wine gives no tag.
        0 | TAG_MOUNT_POINT | TAG_SYMLINK | TAG_LX_SYMLINK | TAG_APPEXECLINK => (Kind::Link, false),
        // A filter driver's folder (cloud files, a projected file system):
        // what it holds is the driver's to give, not entered.
        _ if folder => (Kind::Folder, true),
        // A file of a filter's (deduplicated, compressed, a placeholder).
        _ => (Kind::File, false),
    }
}

/// Whether the mount point `name` in the folder open as `dir` mounts a
/// volume, not a folder (a junction).
fn mounts_a_volume(dir: &Handle, name: &[u8]) -> bool {
    let wide = crate::wtf8::decode(name);
    // Its own name alone below `dir`: no reparse point to cross.
    let Ok(point) = nt_open(dir.0, &wide) else {
        // What cannot be read is not entered either.
        return true;
    };
    let mut buffer = vec![0u64; 2048];
    let mut written = 0u32;
    // SAFETY: the output buffer and its length in bytes; no input; the
    // count written is ours.
    let ok = unsafe {
        DeviceIoControl(
            point.0,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buffer.as_mut_ptr().cast::<c_void>(),
            u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX),
            &raw mut written,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return true;
    }
    let bytes = &bytemuck(&buffer)[..written as usize];
    target_is_volume(bytes).unwrap_or(true)
}

/// Whether a mount point's reparse data names a volume (`\??\Volume{`),
/// not a folder: none if the data cannot be read.
fn target_is_volume(data: &[u8]) -> Option<bool> {
    // The tag, the data's length and a reserved field, then the
    // substitute name's offset and length, in bytes from the path buffer
    // at 16.
    let u16_at = |o: usize| Some(u16::from_le_bytes(data.get(o..o + 2)?.try_into().ok()?));
    let offset = usize::from(u16_at(8)?);
    let length = usize::from(u16_at(10)?);
    let name = data.get(16 + offset..16 + offset + length)?;
    let wide: Vec<u16> = name
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let volume: Vec<u16> = r"\??\Volume{".encode_utf16().collect();
    Some(wide.starts_with(&volume))
}

fn why_not_opened(fail: Fail) -> Why {
    match fail {
        Fail::Gone => Why::Gone,
        Fail::Denied => Why::Denied,
        Fail::Replaced => Why::Replaced,
        Fail::Other(n) => Why::Failed(n),
    }
}

/// Read the folder at `rel` below the root: its stat first, then its
/// entries, each with what its listing says (no stat for each).
pub(crate) fn read(
    root: &Root,
    rel: &Path,
    opts: &Options,
    _: &mut Watch,
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
    let handle = match root.open_folder(rel) {
        Ok(handle) => handle,
        Err(fail) => return not_covered(None, why_not_opened(fail)),
    };
    let (stat, attributes) = match stat_of(&handle) {
        Ok(stat) => stat,
        Err(e) => return not_covered(None, Why::Failed(e.raw_os_error().unwrap_or(0))),
    };
    // A reparse point opened as itself: a link put where the folder was.
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return not_covered(Some(stat), Why::Replaced);
    }
    if root.exclusions.holds(Some(stat.id), &root.path.join(rel)) {
        return not_covered(Some(stat), Why::Excluded);
    }
    if stat.mount != root.stat.mount {
        return not_covered(Some(stat), Why::Mount);
    }
    let mut items = match list(&handle) {
        Ok(items) => items,
        Err(fail) => return not_covered(Some(stat), Why::CutShort(fail.number())),
    };
    if items.iter().any(|i| i.name == b".private") {
        return not_covered(Some(stat), Why::Private);
    }
    let again = read_at < stat.ctime.plus(opts.tick);
    items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let mut listing = Listing::new(stat, read_at, again);
    for item in &items {
        let (kind, mount) = classify(&handle, item);
        let id = FileId {
            dev: stat.id.dev,
            ino: item.id,
        };
        if mount {
            root.found_mount(rel.join(crate::wtf8::path(&item.name)));
            listing.push(
                &item.name,
                Entry {
                    kind,
                    id,
                    attrs: None,
                },
            );
            continue;
        }
        if let Some(entry) = reuse.and_then(|r| r(&item.name, item.id, kind)) {
            listing.push(&item.name, entry);
            continue;
        }
        let attrs = Attrs {
            size: item.size,
            alloc: item.alloc,
            mtime: item.mtime,
            ctime: item.ctime,
        };
        listing.push(
            &item.name,
            Entry {
                kind,
                id,
                attrs: opts.attrs.then_some(attrs),
            },
        );
    }
    (Some(stat), Outcome::Listed(listing))
}

/// Whether the stored folder at `rel` is current: opened beneath the
/// root, its identity, volume and change time compared through its own
/// handle (a listing's copy of a folder's times may lag on NTFS).
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
    let handle = match root.open_folder(rel) {
        Ok(handle) => handle,
        Err(fail) => {
            return Some(match fail {
                Fail::Gone => Stale::Gone,
                Fail::Denied => Stale::Denied,
                Fail::Replaced => Stale::Replaced,
                Fail::Other(n) => Stale::Failed(n),
            });
        }
    };
    match stat_of(&handle) {
        Err(e) => Some(Stale::Failed(e.raw_os_error().unwrap_or(0))),
        Ok((_, attributes)) if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
            Some(Stale::Replaced)
        }
        Ok((st, _)) if st.mount != root.stat.mount => Some(Stale::Mount),
        Ok((st, _)) if st.id != stored.id => Some(Stale::Replaced),
        Ok((st, _)) if st.ctime != stored.ctime => Some(Stale::Changed),
        Ok(_) => None,
    }
}

/// [`verdict`]: no watcher on Windows yet (phase 4, step 5).
pub(crate) fn verdict_watched(
    root: &Root,
    points: &[(PathBuf, bool)],
    rel: &Path,
    stored: StoredFolder,
    _: &Options,
) -> (Option<Stale>, Watch) {
    (verdict(root, points, rel, stored), Watch::Unasked)
}

/// A name's bytes, its WTF-8, as a path component.
pub(crate) fn component(name: &[u8]) -> PathBuf {
    crate::wtf8::path(name)
}

/// `path`'s bytes, its WTF-8, put at the end of `buf`.
pub(crate) fn push_path(buf: &mut Vec<u8>, path: &Path) {
    crate::wtf8::push(path, buf);
}

/// The path `bytes` name, as [`push_path`] put them.
pub(crate) fn path_of(bytes: &[u8]) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Owned(crate::wtf8::path(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mount point's data says whether it mounts a volume or is a
    /// junction to a folder.
    #[test]
    fn a_mount_points_target_says_volume_or_folder() {
        let data = |target: &str| {
            let name: Vec<u8> = target.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let len = u16::try_from(name.len()).unwrap();
            let mut out = Vec::new();
            out.extend(TAG_MOUNT_POINT.to_le_bytes());
            out.extend((u16::try_from(name.len() + 12).unwrap()).to_le_bytes());
            out.extend([0, 0]);
            // The substitute name first, then an empty print name.
            out.extend(0u16.to_le_bytes());
            out.extend(len.to_le_bytes());
            out.extend(len.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name);
            out
        };
        let volume = r"\??\Volume{0a1b2c3d-0000-0000-0000-100000000000}\";
        assert_eq!(target_is_volume(&data(volume)), Some(true));
        assert_eq!(
            target_is_volume(&data(r"\??\C:\Users\someone\data")),
            Some(false)
        );
        assert_eq!(target_is_volume(&data(volume)[..12]), None, "cut short");
    }

    /// A file time is an instant since 1970, 100 ns at a time.
    #[test]
    fn a_file_time_is_an_instant() {
        assert_eq!(time(116_444_736_000_000_000), Time { sec: 0, nsec: 0 });
        assert_eq!(
            time(116_444_736_000_000_000 + 15_000_001),
            Time {
                sec: 1,
                nsec: 500_000_100
            }
        );
        assert_eq!(
            time(116_444_736_000_000_000 - 5_000_000),
            Time {
                sec: -1,
                nsec: 500_000_000
            }
        );
    }

    /// A path is named plainly, and opened in its verbatim form.
    #[test]
    fn a_path_is_named_plainly_and_opened_verbatim() {
        assert_eq!(
            plain(Path::new(r"\\?\C:\Users\someone")),
            Path::new(r"C:\Users\someone")
        );
        assert_eq!(plain(Path::new(r"C:\Users")), Path::new(r"C:\Users"));
        let w = |p: &str| String::from_utf16(&verbatim(Path::new(p))).unwrap();
        assert_eq!(w(r"C:\Users"), "\\\\?\\C:\\Users\0");
        assert_eq!(w(r"\\?\C:\Users"), "\\\\?\\C:\\Users\0");
        assert_eq!(w(r"\\server\share\a"), "\\\\?\\UNC\\server\\share\\a\0");
        assert_eq!(w("C:/a/b"), "\\\\?\\C:\\a\\b\0", "separators made `\\`");
        assert_eq!(w(r"C:\"), "\\\\?\\C:\\\0", "a drive's root");
    }
}
