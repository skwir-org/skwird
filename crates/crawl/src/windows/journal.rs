//! An NTFS volume's file table and change journal (`docs/DESIGN.md`,
//! decisions 2 and 3; `docs/PLAN.md`, phase 4, step 7): what the system
//! service builds from and catches up with, as Everything does. Both need
//! a handle on the volume itself, which administrators alone may open.
//!
//! - **The file table** ([`Volume::enumerate`]): a record for each file and
//!   folder, its id, its parent's, its name and its attributes, read in
//!   batches (`FSCTL_ENUM_USN_DATA`); assembled into the tree its parent
//!   links make ([`Table`]), from the root folder's id down.
//! - **The journal** ([`Volume::journal`], [`Volume::read`]): its identity and bounds
//!   (`FSCTL_QUERY_USN_JOURNAL`), and its records from a position on
//!   (`FSCTL_READ_USN_JOURNAL`), each naming the folder whose entry
//!   changed by its parent's id.
//! - **A position kept is used only where the journal still holds it**
//!   ([`Position::holds`]): the same journal (its id; one deleted and made
//!   again has another), at or past the lowest record it keeps, not past
//!   its next. Otherwise the check runs (DESIGN 2).
//!
//! Records of versions 2 (64-bit ids, NTFS's) and 3 (128-bit, `ReFS`'s)
//! are read alike, by their own lengths and offsets.
//!
//! Wine has no change journal nor file table to read: the records'
//! parsing, the tree's assembly and the position's rule are tested on
//! bytes and records of the tests' making; the calls, on Windows alone.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_HANDLE_EOF, GENERIC_READ, GetLastError};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0,
    USN_JOURNAL_DATA_V0,
};

use super::Handle;
use crate::Time;

/// A file's id in a record: NTFS's 64 bits, or `ReFS`'s 128.
pub type Id = u128;

/// A journal's identity and bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Journal {
    /// Its id: another for a journal deleted and made again.
    pub id: u64,
    /// The first record it ever held.
    pub first: i64,
    /// The number its next record will take.
    pub next: i64,
    /// The lowest record it still holds: those below are gone.
    pub lowest_valid: i64,
}

/// Where reading a journal goes on from: kept with the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// The journal it was read from.
    pub journal: u64,
    /// The next record to read.
    pub next: i64,
}

impl Position {
    /// Whether reading on from here misses nothing (DESIGN 2): the same
    /// journal, at or past the lowest record it holds, not past its next.
    /// A journal made again, or one that wrapped past the position, or a
    /// position from ahead of it (a volume restored from an image), holds
    /// it not: the check runs instead.
    pub fn holds(&self, journal: &Journal) -> bool {
        self.journal == journal.id && self.next >= journal.lowest_valid && self.next <= journal.next
    }
}

/// A file table's or a journal's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The record's number in the journal (none in a file table's).
    pub usn: i64,
    pub id: Id,
    /// The folder holding it.
    pub parent: Id,
    /// Why it was written (`USN_REASON_*`): what changed.
    pub reason: u32,
    pub attributes: u32,
    /// Its name's WTF-8 ([`crate::wtf8`]).
    pub name: Vec<u8>,
    pub time: Time,
}

impl Record {
    pub fn is_folder(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// The records a buffer holds after its first 8 bytes (the next position,
/// which [`next_of`] reads): each by its own length, its name by its own
/// offset. A record cut short, or of a version not known, ends them.
pub fn records(buffer: &[u8]) -> Vec<Record> {
    records_whole(buffer).0
}

/// [`records`], and whether they are all the buffer holds: none cut
/// short, none of a version not known, nothing left after the last.
pub fn records_whole(buffer: &[u8]) -> (Vec<Record>, bool) {
    let mut out = Vec::new();
    let mut at = 8;
    while let Some(head) = buffer.get(at..at + 8) {
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let major = u16::from_le_bytes([head[4], head[5]]);
        let Some(bytes) = buffer.get(at..at + len).filter(|_| len >= 8) else {
            return (out, false);
        };
        let Some(record) = record(bytes, major) else {
            return (out, false);
        };
        out.push(record);
        at += len;
    }
    (out, at >= buffer.len())
}

/// The next position a buffer's first 8 bytes say: the journal's next
/// record, or the file table's next id.
pub fn next_of(buffer: &[u8]) -> Option<i64> {
    Some(i64::from_le_bytes(buffer.get(..8)?.try_into().ok()?))
}

/// One record of version 2 or 3, its fields where its version puts them.
fn record(bytes: &[u8], major: u16) -> Option<Record> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(bytes.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?));
    let i64_at = |o: usize| Some(i64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?));
    let id_at = |o: usize, wide: bool| -> Option<Id> {
        Some(if wide {
            u128::from_le_bytes(bytes.get(o..o + 16)?.try_into().ok()?)
        } else {
            u128::from(u64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?))
        })
    };
    // Where each field is: version 3's ids take 16 bytes each.
    let (wide, rest) = match major {
        2 => (false, 24),
        3 => (true, 40),
        _ => return None,
    };
    let (len, offset) = (u16_at(rest + 32)?, u16_at(rest + 34)?);
    let name = bytes.get(usize::from(offset)..usize::from(offset) + usize::from(len))?;
    let wide_name = name
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]));
    let mut wtf8 = Vec::with_capacity(name.len());
    crate::wtf8::encode(wide_name, &mut wtf8);
    Some(Record {
        id: id_at(8, wide)?,
        parent: id_at(if wide { 24 } else { 16 }, wide)?,
        usn: i64_at(rest)?,
        time: super::time(i64_at(rest + 8)?),
        reason: u32_at(rest + 16)?,
        attributes: u32_at(rest + 28)?,
        name: wtf8,
    })
}

/// A file table, assembled: each folder's entries by their parent links.
#[derive(Debug, Default)]
pub struct Table {
    records: HashMap<Id, Record>,
    children: HashMap<Id, Vec<Id>>,
}

impl Table {
    /// The table the records make. A file of several names (hard links)
    /// has one record per name in the file table, all of one id: the last
    /// is kept, as the table holds a name for each id alone.
    pub fn of(records: impl IntoIterator<Item = Record>) -> Table {
        let mut table = Table::default();
        for r in records {
            table.records.insert(r.id, r);
        }
        for r in table.records.values() {
            table.children.entry(r.parent).or_default().push(r.id);
        }
        for ids in table.children.values_mut() {
            ids.sort_unstable();
        }
        table
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every entry below the folder of id `root`, each with its path below
    /// it, each folder before what it holds. An entry no parent link leads
    /// to from `root` is not among them ([`Table::orphans`]); nor is one
    /// met twice, should the links make a loop.
    pub fn below(&self, root: Id) -> Vec<(PathBuf, &Record)> {
        let mut out = Vec::new();
        let mut seen = HashSet::from([root]);
        let mut next = vec![(PathBuf::new(), root)];
        while let Some((at, folder)) = next.pop() {
            for id in self.children.get(&folder).into_iter().flatten() {
                if !seen.insert(*id) {
                    continue;
                }
                let record = &self.records[id];
                let path = at.join(crate::wtf8::path(&record.name));
                if record.is_folder() {
                    next.push((path.clone(), *id));
                }
                out.push((path, record));
            }
        }
        out
    }

    /// The records no parent link leads to from `root`: on the volume, but
    /// below no folder of the root's (or the volume's metadata files).
    pub fn orphans(&self, root: Id) -> Vec<&Record> {
        let reached: HashSet<Id> = self.below(root).iter().map(|(_, r)| r.id).collect();
        let mut out: Vec<&Record> = self
            .records
            .values()
            .filter(|r| r.id != root && !reached.contains(&r.id))
            .collect();
        out.sort_unstable_by_key(|r| r.id);
        out
    }
}

/// A volume, opened itself: administrators alone may open one.
pub struct Volume(pub(crate) Handle);

impl Volume {
    /// The volume at `volume` (`C:`, or a volume's GUID path).
    pub fn open(volume: &OsStr) -> std::io::Result<Volume> {
        open_volume(volume).map(Volume)
    }

    /// Its journal: its identity and bounds.
    pub fn journal(&self) -> std::io::Result<Journal> {
        query(&self.0)
    }

    /// Its journal's records from `from` on, as many as `buffer` holds,
    /// and the position to read on from.
    pub fn read(
        &self,
        from: Position,
        buffer: &mut [u64],
    ) -> std::io::Result<(Position, Vec<Record>)> {
        read(&self.0, from, buffer)
    }

    /// Every record of its file table, its journal's records up to `upto`
    /// taken in.
    pub fn enumerate(&self, upto: i64, buffer: &mut [u64]) -> std::io::Result<Vec<Record>> {
        enumerate(&self.0, upto, buffer)
    }
}

impl Volume {
    /// The volume holding `path`, whatever letter or folder it is mounted
    /// at.
    pub fn of(path: &Path) -> std::io::Result<Volume> {
        open_device(&device_of(path)?, 0).map(Volume)
    }

    /// A handle on the volume holding `path`, for the caller to keep: the
    /// system service registers it to be told when the volume's removal is
    /// asked, and closes it then.
    pub fn handle_of(path: &Path) -> std::io::Result<std::os::windows::io::OwnedHandle> {
        use std::os::windows::io::FromRawHandle;
        let handle = open_device(&device_of(path)?, 0)?;
        let raw = handle.0;
        std::mem::forget(handle);
        // SAFETY: a handle of ours alone, open, given away whole.
        Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) })
    }

    /// Make its journal, where it has none (`FSCTL_CREATE_USN_JOURNAL`):
    /// at most `max` bytes, grown and cut by `delta`. One there already is
    /// kept, its records with it.
    pub fn create_journal(&self, max: u64, delta: u64) -> std::io::Result<()> {
        let input = CREATE_USN_JOURNAL_DATA {
            MaximumSize: max,
            AllocationDelta: delta,
        };
        control(&self.0, FSCTL_CREATE_USN_JOURNAL, &input, &mut []).map(|_| ())
    }
}

/// The device of the volume holding `path` (`\\?\Volume{…}`),
/// NUL-terminated: its mount point found (`C:\`, or a folder another
/// volume is mounted at), then the volume mounted there.
pub(crate) fn device_of(path: &Path) -> std::io::Result<Vec<u16>> {
    let name = super::verbatim(path);
    let mut point = vec![0u16; name.len() + 2];
    // SAFETY: a NUL-terminated name; the buffer and its length in units,
    // as long as the name, which its mount point is a part of.
    let ok = unsafe {
        GetVolumePathNameW(
            name.as_ptr(),
            point.as_mut_ptr(),
            u32::try_from(point.len()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut device = vec![0u16; 64];
    // SAFETY: the mount point, NUL-terminated by the call; the buffer and
    // its length, more than a volume's name (`\\?\Volume{GUID}\`) takes.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(
            point.as_ptr(),
            device.as_mut_ptr(),
            u32::try_from(device.len()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Without its last `\`, the name opens the volume, not its root folder.
    let end = device.iter().position(|&u| u == 0).unwrap_or(device.len());
    device.truncate(end);
    if device.last() == Some(&u16::from(b'\\')) {
        device.pop();
    }
    device.push(0);
    Ok(device)
}

fn open_volume(volume: &OsStr) -> std::io::Result<Handle> {
    let mut name: Vec<u16> = OsStr::new(r"\\.\").encode_wide().collect();
    name.extend(volume.encode_wide());
    name.push(0);
    open_device(&name, 0)
}

/// The volume device `device` names (NUL-terminated), opened to read, with
/// `flags` (`FILE_FLAG_OVERLAPPED` for a read that waits).
pub(crate) fn open_device(device: &[u16], flags: u32) -> std::io::Result<Handle> {
    open_device_as(device, GENERIC_READ, flags)
}

/// The volume device `device` names (NUL-terminated), opened with `access`
/// and `flags`.
pub(crate) fn open_device_as(device: &[u16], access: u32, flags: u32) -> std::io::Result<Handle> {
    // SAFETY: a NUL-terminated name alive for the call, no security
    // attributes, no template; the handle given is ours to close.
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Handle(handle))
}

/// One control call on the volume: `input` given, the bytes written to
/// `output` given back.
pub(super) fn control<T>(
    volume: &Handle,
    code: u32,
    input: &T,
    output: &mut [u64],
) -> std::io::Result<usize> {
    let mut written = 0u32;
    // SAFETY: the input, its size; the output buffer, aligned, and its
    // length in bytes; the count written ours. Nothing pending: the call
    // is synchronous on a handle opened so.
    let ok = unsafe {
        DeviceIoControl(
            volume.0,
            code,
            std::ptr::from_ref(input).cast::<c_void>(),
            u32::try_from(size_of::<T>()).unwrap_or(u32::MAX),
            output.as_mut_ptr().cast::<c_void>(),
            u32::try_from(output.len() * 8).unwrap_or(u32::MAX),
            &raw mut written,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(written as usize)
}

fn query(volume: &Handle) -> std::io::Result<Journal> {
    let mut out = [0u64; size_of::<USN_JOURNAL_DATA_V0>().div_ceil(8)];
    control(volume, FSCTL_QUERY_USN_JOURNAL, &(), &mut out)?;
    // SAFETY: the call wrote the structure at the buffer's start, aligned.
    let data = unsafe { &*out.as_ptr().cast::<USN_JOURNAL_DATA_V0>() };
    Ok(Journal {
        id: data.UsnJournalID,
        first: data.FirstUsn,
        next: data.NextUsn,
        lowest_valid: data.LowestValidUsn,
    })
}

fn read(
    volume: &Handle,
    from: Position,
    buffer: &mut [u64],
) -> std::io::Result<(Position, Vec<Record>)> {
    let input = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: from.next,
        // Every reason.
        ReasonMask: u32::MAX,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: from.journal,
    };
    let written = control(volume, FSCTL_READ_USN_JOURNAL, &input, buffer)?;
    let bytes = &super::bytemuck(buffer)[..written];
    let next = next_of(bytes).unwrap_or(from.next);
    Ok((
        Position {
            journal: from.journal,
            next,
        },
        records(bytes),
    ))
}

/// Read in batches through `buffer`, from the first id on.
fn enumerate(volume: &Handle, upto: i64, buffer: &mut [u64]) -> std::io::Result<Vec<Record>> {
    let mut all = Vec::new();
    let mut input = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: upto,
    };
    loop {
        let written = match control(volume, FSCTL_ENUM_USN_DATA, &input, buffer) {
            Ok(n) => n,
            // SAFETY: no argument.
            Err(_) if unsafe { GetLastError() } == ERROR_HANDLE_EOF => return Ok(all),
            Err(e) => return Err(e),
        };
        let bytes = &super::bytemuck(buffer)[..written];
        let batch = records(bytes);
        let Some(next) = next_of(bytes) else {
            return Ok(all);
        };
        if batch.is_empty() {
            return Ok(all);
        }
        all.extend(batch);
        // The file table's next id: the same 8 bytes, unsigned.
        input.StartFileReferenceNumber = u64::from_le_bytes(next.to_le_bytes());
    }
}

/// A record's name, for a test's records.
#[cfg(test)]
fn wtf8_of(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    crate::wtf8::encode(name.encode_utf16(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as the volume writes it, of `major` version 2 or 3.
    fn raw(
        major: u16,
        id: Id,
        parent: Id,
        usn: i64,
        reason: u32,
        folder: bool,
        name: &str,
    ) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut out = Vec::new();
        out.extend(0u32.to_le_bytes()); // its length, written below
        out.extend(major.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        if major == 3 {
            out.extend(id.to_le_bytes());
            out.extend(parent.to_le_bytes());
        } else {
            out.extend(u64::try_from(id).unwrap().to_le_bytes());
            out.extend(u64::try_from(parent).unwrap().to_le_bytes());
        }
        out.extend(usn.to_le_bytes());
        // 1970, as a file time.
        out.extend(116_444_736_000_000_000_i64.to_le_bytes());
        out.extend(reason.to_le_bytes());
        out.extend(0u32.to_le_bytes()); // its source
        out.extend(0u32.to_le_bytes()); // its security's id
        let attributes = if folder {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            0x20
        };
        out.extend(attributes.to_le_bytes());
        out.extend(u16::try_from(wide.len()).unwrap().to_le_bytes());
        let offset = u16::try_from(out.len() + 2).unwrap();
        out.extend(offset.to_le_bytes());
        out.extend(wide);
        // Records are aligned on 8 bytes.
        out.resize(out.len().next_multiple_of(8), 0);
        let len = u32::try_from(out.len()).unwrap().to_le_bytes();
        out[..4].copy_from_slice(&len);
        out
    }

    /// Records of both versions read back as written, after the buffer's
    /// next position; one cut short ends them, never read past.
    #[test]
    fn records_of_both_versions_read_back() {
        let mut buffer = 77_i64.to_le_bytes().to_vec();
        buffer.extend(raw(
            2,
            0x0005_0000_0000_0040,
            0x0005_0000_0000_0005,
            9,
            0x100,
            false,
            "notes.txt",
        ));
        let wide_id = 0x1234_5678_9abc_def0_1122_3344_5566_7788_u128;
        buffer.extend(raw(3, wide_id, 0x99, 10, 0x200, true, "été 東京"));
        assert_eq!(next_of(&buffer), Some(77));
        let read = records(&buffer);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].id, 0x0005_0000_0000_0040);
        assert_eq!(read[0].parent, 0x0005_0000_0000_0005);
        assert_eq!((read[0].usn, read[0].reason), (9, 0x100));
        assert_eq!(read[0].name, b"notes.txt");
        assert!(!read[0].is_folder());
        assert_eq!(read[0].time, Time { sec: 0, nsec: 0 });
        assert_eq!((read[1].id, read[1].parent), (wide_id, 0x99));
        assert_eq!(read[1].name, wtf8_of("été 東京"));
        assert!(read[1].is_folder());
        let cut = &buffer[..buffer.len() - 4];
        assert_eq!(records(cut).len(), 1, "the record cut short is not read");
        assert!(records_whole(&buffer).1, "read whole");
        assert!(!records_whole(cut).1, "the record cut short is said");
        let mut unknown = buffer.clone();
        unknown.extend(raw(4, 1, 2, 11, 0, false, "v4"));
        assert_eq!(records_whole(&unknown).0.len(), 2);
        assert!(!records_whole(&unknown).1, "a version not known is said");
        assert!(records_whole(&77_i64.to_le_bytes()).1, "no record: whole");
    }

    /// A position kept is used only where its journal still holds it.
    #[test]
    fn a_position_holds_only_where_the_journal_keeps_it() {
        let journal = Journal {
            id: 7,
            first: 0,
            next: 5000,
            lowest_valid: 1000,
        };
        let at = |journal: u64, next: i64| Position { journal, next };
        assert!(at(7, 1000).holds(&journal), "its lowest record");
        assert!(at(7, 3000).holds(&journal));
        assert!(at(7, 5000).holds(&journal), "nothing new yet");
        assert!(!at(7, 999).holds(&journal), "wrapped past it");
        assert!(!at(7, 5001).holds(&journal), "ahead of the journal");
        assert!(!at(8, 3000).holds(&journal), "another journal");
    }

    fn rec(id: Id, parent: Id, folder: bool, name: &str) -> Record {
        Record {
            usn: 0,
            id,
            parent,
            reason: 0,
            attributes: if folder {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                0x20
            },
            name: wtf8_of(name),
            time: Time::default(),
        }
    }

    /// The tree the parent links make, from a root's id down, each folder
    /// before what it holds; what no link leads to from it set apart, and
    /// a loop of links never followed twice.
    #[test]
    fn the_table_is_the_tree_its_links_make() {
        let root = 5;
        let table = Table::of([
            rec(10, root, true, "Users"),
            rec(11, 10, true, "someone"),
            rec(12, 11, false, "notes.txt"),
            rec(13, root, false, "pagefile.sys"),
            // Elsewhere on the volume: its parent is not reached.
            rec(20, 99, false, "lost.txt"),
            // A loop: each the other's parent.
            rec(30, 31, true, "a"),
            rec(31, 30, true, "b"),
        ]);
        let below = table.below(root);
        let paths: Vec<PathBuf> = below.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(paths.len(), 4, "{paths:?}");
        for p in [
            r"Users",
            r"Users\someone",
            r"Users\someone\notes.txt",
            "pagefile.sys",
        ] {
            assert!(paths.contains(&PathBuf::from(p)), "{p}: {paths:?}");
        }
        let at = |p: &str| paths.iter().position(|q| q == &PathBuf::from(p)).unwrap();
        assert!(
            at("Users") < at(r"Users\someone"),
            "a folder before what it holds"
        );
        let orphans: Vec<Id> = table.orphans(root).iter().map(|r| r.id).collect();
        assert_eq!(orphans, vec![20, 30, 31]);
        // From inside the loop, each is met once.
        assert_eq!(table.below(30).len(), 1);
    }
}
