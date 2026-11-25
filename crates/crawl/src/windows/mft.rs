//! An NTFS volume's file table, read whole from the volume itself
//! (`docs/DESIGN.md`, decision 2; `docs/PLAN.md`, phase 4, step 7, "the
//! fast way", the user's choice): what the system service builds its
//! index from, as Everything does. One sequential read of the volume's
//! own records, where a walk opens every folder and asks it.
//!
//! - **Where the table is** ([`Volume::layout`]): the volume's record and
//!   cluster sizes, the table's first cluster and its length in use
//!   (`FSCTL_GET_NTFS_VOLUME_DATA`). The table's first record describes
//!   the table itself: its data's runs say which clusters hold it.
//! - **A record** ([`decode`]): 1 KiB (4 KiB on some disks), each 512
//!   bytes of it ending with the record's update number, the bytes it
//!   hid kept in the record's head ([`fixup`]). A sector whose end is not
//!   that number was written apart from the rest: the record is torn, as
//!   one being written while it is read is, and is not trusted.
//! - **What a record says**, by its attributes: its times, attributes and
//!   security id (`$STANDARD_INFORMATION`), each of its names and the
//!   folder holding it (`$FILE_NAME`, one for each hard link; the short
//!   DOS names left out, as a listing leaves them), its data's size
//!   (`$DATA`), its reparse tag (`$REPARSE_POINT`). A file whose
//!   attributes overflow its record has others, which name it their base
//!   ([`Table::of`] merges them).
//! - **The tree** ([`Table`]): every record's names placed in the folders
//!   they name, a folder's link to its parent kept only where the
//!   parent's record is still the same file (its sequence number), as a
//!   record reused for another file has another.
//!
//! The volume's metadata (records 0 to 15, but the root folder's, and
//! what `$Extend` holds) is never an entry: a listing never shows it.
//!
//! **Read from the disk, not through the file system:** what NTFS holds
//! in its cache and has not written yet is not read. The system service
//! takes its journal's position before it reads the table, and what the
//! journal says changed after it is read again from its folders
//! (`docs/AGENTS.md`, "Watch, then open, then list"), so nothing changed
//! meanwhile is missed.

#![allow(clippy::cast_possible_truncation)] // Offsets read from a record are bounded by its length, checked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, ReadFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

use super::journal::{Volume, control};
use super::{Folder, Item, Root, folder_kind, listed};
use crate::Time;
use crate::walk::mount_why;
use crate::{Checked, FileId, FolderStat, Options, Outcome, Stale, Watch, Why};

/// A file's reference: its record's number (48 bits) and that record's
/// sequence number (16), which changes when the record is reused.
pub type Reference = u64;

/// A reference's record number.
pub fn number(reference: Reference) -> u64 {
    reference & 0x0000_FFFF_FFFF_FFFF
}

/// A reference's sequence number.
pub fn sequence(reference: Reference) -> u16 {
    (reference >> 48) as u16
}

/// The root folder's record.
pub const ROOT: u64 = 5;
/// The records below this one are the volume's metadata, the root's but.
const FIRST_FILE: u64 = 16;
/// Every sector of a record ends with its update number, every 512 bytes
/// whatever the disk's sectors.
const STRIDE: usize = 512;

const STANDARD_INFORMATION: u32 = 0x10;
const ATTRIBUTE_LIST: u32 = 0x20;
const FILE_NAME: u32 = 0x30;
const DATA: u32 = 0x80;
const REPARSE_POINT: u32 = 0xC0;
const END: u32 = 0xFFFF_FFFF;
/// A `$FILE_NAME`'s namespace: the short name DOS sees, beside a long one.
const DOS: u8 = 2;

/// Why a record is not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// Never used (zeroes), or not a record.
    Empty,
    /// In no use now: a file deleted.
    Unused,
    /// A sector written apart from the others, or marked bad by NTFS.
    Torn,
    /// An attribute past the record's end, a field out of its bounds.
    Malformed,
}

/// One of a file's names, and the folder holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub parent: Reference,
    /// Its WTF-8 ([`crate::wtf8`]).
    pub name: Vec<u8>,
}

/// What one record says, before a file's records are merged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub number: u64,
    pub sequence: u16,
    /// The base record's reference, for a record holding attributes its
    /// base had no room for; 0 for a base record.
    pub base: Reference,
    pub folder: bool,
    pub names: Vec<Name>,
    /// `$STANDARD_INFORMATION`: in a base record alone.
    pub standard: Option<Standard>,
    /// The unnamed `$DATA`'s size and allocation, where its first part is.
    pub data: Option<(u64, u64)>,
    pub tag: Option<u32>,
    /// Its reparse tag as a name's attribute keeps it, for a reparse point
    /// whose data is too large for its record.
    pub name_tag: Option<u32>,
    /// For a mount point, whether it mounts a volume (not a folder, as a
    /// junction does), as its reparse data says: none where the data is not
    /// in the record, and a mount point is then taken to, and not entered.
    pub volume: Option<bool>,
    /// Whether it has an `$ATTRIBUTE_LIST`: other records hold some of its
    /// attributes.
    pub listed: bool,
    /// Its unnamed `$DATA`'s runs, where they are in this record from its
    /// start (the table's own first record says where the table is).
    pub runs: Option<Vec<Run>>,
}

/// `$STANDARD_INFORMATION`, as a listing gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standard {
    /// Its data's last write (`LastWriteTime`), as a file time.
    pub mtime: i64,
    /// Its record's last change (`ChangeTime`).
    pub ctime: i64,
    pub attributes: u32,
    /// Its security descriptor's id in `$Secure`, where the volume keeps
    /// them (NTFS 3.0 on).
    pub security: Option<u32>,
}

/// A run of a non-resident attribute's clusters: where on the volume (none
/// for a sparse run, which holds zeroes), and how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// A file, its records merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub reference: Reference,
    pub folder: bool,
    pub names: Vec<Name>,
    pub attributes: u32,
    pub security: Option<u32>,
    pub mtime: Time,
    pub ctime: Time,
    pub size: u64,
    pub alloc: u64,
    /// Its reparse tag, 0 for none.
    pub tag: u32,
    /// A mount point that mounts a volume ([`Decoded::volume`]).
    pub volume: bool,
}

impl Record {
    /// A reparse point: a link, a junction, a mount, a filter driver's.
    pub fn is_reparse(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

/// Restore a record's sector ends from its update sequence array, in
/// place, checking each was the record's update number.
pub fn fixup(record: &mut [u8]) -> Result<(), Bad> {
    let at = usize::from(u16_at(record, 4).ok_or(Bad::Malformed)?);
    let count = usize::from(u16_at(record, 6).ok_or(Bad::Malformed)?);
    let sectors = count.checked_sub(1).ok_or(Bad::Malformed)?;
    if sectors == 0 || sectors * STRIDE != record.len() {
        return Err(Bad::Malformed);
    }
    let array = record
        .get(at..at + 2 * count)
        .ok_or(Bad::Malformed)?
        .to_vec();
    for i in 0..sectors {
        let end = (i + 1) * STRIDE;
        if record[end - 2..end] != array[..2] {
            return Err(Bad::Torn);
        }
        record[end - 2..end].copy_from_slice(&array[2 + 2 * i..4 + 2 * i]);
    }
    Ok(())
}

/// Record `number` as read from the table: checked, restored, decoded.
pub fn decode(number: u64, record: &mut [u8]) -> Result<Decoded, Bad> {
    signed(record)?;
    fixup(record)?;
    decode_fixed(number, record)
}

/// Whether a record is one, and not marked torn.
fn signed(record: &[u8]) -> Result<(), Bad> {
    match record.get(..4) {
        Some(b"FILE") => Ok(()),
        // NTFS marks a record it found torn so.
        Some(b"BAAD") => Err(Bad::Torn),
        _ => Err(Bad::Empty),
    }
}

/// Record `number` as NTFS holds it in memory, its sector ends restored
/// already, as `FSCTL_GET_NTFS_FILE_RECORD` gives it: decoded.
pub fn decode_fixed(number: u64, record: &[u8]) -> Result<Decoded, Bad> {
    signed(record)?;
    let flags = u16_at(record, 0x16).ok_or(Bad::Malformed)?;
    if flags & 1 == 0 {
        return Err(Bad::Unused);
    }
    let mut out = Decoded {
        number,
        sequence: u16_at(record, 0x10).ok_or(Bad::Malformed)?,
        base: u64_at(record, 0x20).ok_or(Bad::Malformed)?,
        folder: flags & 2 != 0,
        ..Decoded::default()
    };
    let used = (u32_at(record, 0x18).ok_or(Bad::Malformed)? as usize).min(record.len());
    let mut at = usize::from(u16_at(record, 0x14).ok_or(Bad::Malformed)?);
    loop {
        let kind = u32_at(&record[..used], at).ok_or(Bad::Malformed)?;
        if kind == END {
            return Ok(out);
        }
        let len = u32_at(record, at + 4).ok_or(Bad::Malformed)? as usize;
        let attribute = record
            .get(at..at + len)
            .filter(|_| len >= 0x18 && at + len <= used)
            .ok_or(Bad::Malformed)?;
        attribute_into(kind, attribute, &mut out)?;
        at += len;
    }
}

/// One attribute's facts, into what its record says.
fn attribute_into(kind: u32, attribute: &[u8], out: &mut Decoded) -> Result<(), Bad> {
    let resident = attribute[8] == 0;
    let named = attribute[9] != 0;
    let value = || -> Result<&[u8], Bad> {
        let len = u32_at(attribute, 0x10).ok_or(Bad::Malformed)? as usize;
        let at = usize::from(u16_at(attribute, 0x14).ok_or(Bad::Malformed)?);
        attribute.get(at..at + len).ok_or(Bad::Malformed)
    };
    match kind {
        STANDARD_INFORMATION if resident => {
            let v = value()?;
            out.standard = Some(Standard {
                mtime: i64_at(v, 0x08).ok_or(Bad::Malformed)?,
                ctime: i64_at(v, 0x10).ok_or(Bad::Malformed)?,
                attributes: u32_at(v, 0x20).ok_or(Bad::Malformed)?,
                security: u32_at(v, 0x34),
            });
        }
        ATTRIBUTE_LIST => out.listed = true,
        FILE_NAME if resident => {
            let v = value()?;
            let chars = usize::from(*v.get(0x40).ok_or(Bad::Malformed)?);
            if *v.get(0x41).ok_or(Bad::Malformed)? == DOS {
                return Ok(());
            }
            let wide = v.get(0x42..0x42 + 2 * chars).ok_or(Bad::Malformed)?;
            let mut name = Vec::with_capacity(wide.len());
            crate::wtf8::encode(
                wide.chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]])),
                &mut name,
            );
            // A reparse point's tag, in the field that holds a file's
            // extended attributes' size otherwise.
            if u32_at(v, 0x38).is_some_and(|a| a & FILE_ATTRIBUTE_REPARSE_POINT != 0) {
                out.name_tag = u32_at(v, 0x3C);
            }
            out.names.push(Name {
                parent: u64_at(v, 0).ok_or(Bad::Malformed)?,
                name,
            });
        }
        DATA if !named && resident => {
            let len = value()?.len() as u64;
            out.data = Some((len, len.next_multiple_of(8)));
        }
        // Its first part alone says its sizes and where its runs start.
        DATA if !named && u64_at(attribute, 0x10) == Some(0) => {
            let flags = u16_at(attribute, 0x0C).ok_or(Bad::Malformed)?;
            // Compressed or sparse: what is allocated is said further on.
            let alloc_at = if flags & 0x8001 != 0 && attribute.len() >= 0x48 {
                0x40
            } else {
                0x28
            };
            out.data = Some((
                u64_at(attribute, 0x30).ok_or(Bad::Malformed)?,
                u64_at(attribute, alloc_at).ok_or(Bad::Malformed)?,
            ));
            let at = usize::from(u16_at(attribute, 0x20).ok_or(Bad::Malformed)?);
            out.runs = Some(runs(attribute.get(at..).ok_or(Bad::Malformed)?)?);
        }
        // Its data is what FSCTL_GET_REPARSE_POINT gives: the tag first.
        REPARSE_POINT if resident => {
            let data = value()?;
            let tag = u32_at(data, 0).ok_or(Bad::Malformed)?;
            out.tag = Some(tag);
            out.volume = Some(super::target_is_volume(data).unwrap_or(true));
        }
        _ => {}
    }
    Ok(())
}

/// A non-resident attribute's runs: each a header byte (the bytes of its
/// length, low; of its offset, high), its length in clusters, and its
/// start's offset from the run before's, signed; none for a sparse run.
pub fn runs(bytes: &[u8]) -> Result<Vec<Run>, Bad> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut lcn: i64 = 0;
    loop {
        let head = *bytes.get(at).ok_or(Bad::Malformed)?;
        if head == 0 {
            return Ok(out);
        }
        let (len_bytes, offset_bytes) = (usize::from(head & 0xF), usize::from(head >> 4));
        if len_bytes == 0 || len_bytes > 8 || offset_bytes > 8 {
            return Err(Bad::Malformed);
        }
        let field = bytes
            .get(at + 1..at + 1 + len_bytes + offset_bytes)
            .ok_or(Bad::Malformed)?;
        let mut clusters = [0u8; 8];
        clusters[..len_bytes].copy_from_slice(&field[..len_bytes]);
        let run = if offset_bytes == 0 {
            None
        } else {
            let delta = &field[len_bytes..];
            // Signed: its last byte's high bit extends.
            let fill = if delta[offset_bytes - 1] & 0x80 == 0 {
                0
            } else {
                0xFF
            };
            let mut offset = [fill; 8];
            offset[..offset_bytes].copy_from_slice(delta);
            lcn = lcn
                .checked_add(i64::from_le_bytes(offset))
                .ok_or(Bad::Malformed)?;
            Some(u64::try_from(lcn).map_err(|_| Bad::Malformed)?)
        };
        out.push(Run {
            lcn: run,
            clusters: u64::from_le_bytes(clusters),
        });
        at += 1 + len_bytes + offset_bytes;
    }
}

/// The table's files, their records merged, and the tree their names make.
#[derive(Debug, Default)]
pub struct Table {
    records: HashMap<u64, Record>,
    /// For each folder's record, its entries: a record's number, and which
    /// of its names.
    children: HashMap<u64, Vec<(u64, usize)>>,
    /// The records not read: torn (read twice so), marked bad, or
    /// malformed. What they hold is not known, nor which folders hold
    /// them: a crawl is never read from a table holding one
    /// ([`TableReader`]). A check by it is sound all the same, each folder
    /// judged by its own record, and one whose record is unread found
    /// nowhere, so read again from the disk.
    pub unread: Vec<u64>,
    /// When its read began: a folder changed within a tick of it is read
    /// again at the next check, as one listed so is.
    pub read_at: Time,
    /// Its volume's serial number, every identity's first half.
    pub serial: u64,
}

impl Table {
    /// The table the decoded records make. A record holding attributes its
    /// base had no room for gives them to its base, if that is still the
    /// same file. A base record with no `$STANDARD_INFORMATION` is unread.
    pub fn of(decoded: impl IntoIterator<Item = Decoded>, mut unread: Vec<u64>) -> Table {
        let mut records = HashMap::new();
        let mut extensions = Vec::new();
        for d in decoded {
            if d.base != 0 {
                extensions.push(d);
                continue;
            }
            let Some(standard) = d.standard else {
                unread.push(d.number);
                continue;
            };
            let (size, alloc) = d.data.unwrap_or_default();
            records.insert(
                d.number,
                Record {
                    reference: d.number | (u64::from(d.sequence) << 48),
                    folder: d.folder,
                    names: d.names,
                    // NTFS keeps a folder's kind in its record's head, not in
                    // its attributes: a listing adds it, and so does the table.
                    attributes: standard.attributes
                        | if d.folder {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            0
                        },
                    security: standard.security,
                    mtime: super::time(standard.mtime),
                    ctime: super::time(standard.ctime),
                    size,
                    alloc,
                    tag: d.tag.or(d.name_tag).unwrap_or(0),
                    volume: d
                        .volume
                        .unwrap_or(d.tag.or(d.name_tag) == Some(super::TAG_MOUNT_POINT)),
                },
            );
        }
        for e in extensions {
            let Some(base) = records.get_mut(&number(e.base)) else {
                continue;
            };
            if sequence(base.reference) != sequence(e.base) {
                continue;
            }
            base.names.extend(e.names);
            if let Some((size, alloc)) = e.data {
                (base.size, base.alloc) = (size, alloc);
            }
            if let Some(tag) = e.tag {
                base.tag = tag;
                base.volume = e.volume.unwrap_or(tag == super::TAG_MOUNT_POINT);
            }
        }
        let mut children: HashMap<u64, Vec<(u64, usize)>> = HashMap::new();
        for (n, r) in &records {
            if *n < FIRST_FILE {
                continue;
            }
            for (i, name) in r.names.iter().enumerate() {
                let parent = number(name.parent);
                let same = records
                    .get(&parent)
                    .is_some_and(|p| p.folder && p.reference == name.parent);
                if same && (parent >= FIRST_FILE || parent == ROOT) {
                    children.entry(parent).or_default().push((*n, i));
                }
            }
        }
        for entries in children.values_mut() {
            entries.sort_unstable();
        }
        unread.sort_unstable();
        Table {
            records,
            children,
            unread,
            read_at: Time::default(),
            serial: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The file `reference` names, if its record is still that file.
    pub fn get(&self, reference: Reference) -> Option<&Record> {
        self.records
            .get(&number(reference))
            .filter(|r| r.reference == reference)
    }

    /// Every entry below the folder `root`, with its path below it, each
    /// folder before what it holds; a file of several names once for each
    /// name below it. A folder is entered once, should links loop.
    pub fn below(&self, root: Reference) -> Vec<(PathBuf, &Record)> {
        let mut out = Vec::new();
        if self.get(root).is_none() {
            return out;
        }
        let mut entered = std::collections::HashSet::from([number(root)]);
        let mut next = vec![(PathBuf::new(), number(root))];
        while let Some((at, folder)) = next.pop() {
            for (n, i) in self.children.get(&folder).into_iter().flatten() {
                let record = &self.records[n];
                let path = at.join(crate::wtf8::path(&record.names[*i].name));
                if record.folder && !record.is_reparse() && entered.insert(*n) {
                    next.push((path.clone(), *n));
                }
                out.push((path, record));
            }
        }
        out
    }
}

impl Table {
    /// The entries of the folder `folder`, as a listing gives them.
    fn items(&self, folder: Reference) -> Vec<Item> {
        let Some(entries) = self.children.get(&number(folder)) else {
            return Vec::new();
        };
        entries
            .iter()
            .map(|(n, i)| {
                let r = &self.records[n];
                Item {
                    name: r.names[*i].name.clone(),
                    attributes: r.attributes,
                    tag: r.tag,
                    id: r.reference,
                    size: r.size,
                    alloc: r.alloc,
                    ctime: r.ctime,
                    mtime: r.mtime,
                    // Its names but the short ones, the base record's and its
                    // others': its hard links, as `nlink` counts them. The
                    // record's own count holds the short names too.
                    links: u32::try_from(r.names.len()).unwrap_or(u32::MAX),
                }
            })
            .collect()
    }

    /// The check ([`crate::check`]) with each folder's stat from the table,
    /// not from the disk: in seconds where a stat of every folder takes
    /// minutes on a volume of millions. Each stored folder is found as its
    /// path finds it, by its name in its parent's entries from the root's
    /// record down, and judged as `windows::verdict` judges it: its
    /// identity, its kind, its change time. A folder the table cannot find
    /// (gone, renamed, a record torn among its parent's) is said gone, and
    /// read again from the disk with the rest that is stale: the check
    /// says no less is current than the disk's would. The folders read
    /// again are read from the disk.
    pub fn check(
        &self,
        root: &Root,
        stored: &crate::Stored,
        cancel: &crate::Cancel,
    ) -> Result<Checked, crate::Error> {
        let start = std::time::Instant::now();
        let points = root.points();
        // Each folder's record, by its path below the root, as found.
        let mut found: HashMap<PathBuf, Option<Reference>> = HashMap::new();
        let on_volume = root.stat.id.dev == self.serial;
        let mut stale = Vec::new();
        for i in 0..stored.len() {
            if cancel.is_cancelled() {
                break;
            }
            let rel = stored.rel(i);
            let reference = if !on_volume {
                None
            } else if rel.as_os_str().is_empty() {
                Some(root.stat.id.ino)
            } else {
                self.find(root, &rel, &mut found)
            };
            if let Some(why) = self.verdict(&points, &rel, stored.folder(i), reference) {
                stale.push((i, why));
            }
        }
        if cancel.is_cancelled() {
            return Err(crate::Error::Cancelled);
        }
        Ok(Checked {
            stale,
            took: start.elapsed(),
        })
    }

    /// The record of the folder at `rel` below the root, found by its name
    /// in its parent's entries, the parent found likewise and remembered in
    /// `found`.
    fn find(
        &self,
        root: &Root,
        rel: &Path,
        found: &mut HashMap<PathBuf, Option<Reference>>,
    ) -> Option<Reference> {
        if let Some(&known) = found.get(rel) {
            return known;
        }
        let parent = rel.parent().unwrap_or(Path::new(""));
        let above = if parent.as_os_str().is_empty() {
            Some(root.stat.id.ino)
        } else {
            self.find(root, parent, found)
        };
        let mut name = Vec::new();
        if let Some(last) = rel.file_name() {
            crate::wtf8::push(Path::new(last), &mut name);
        }
        let reference = above.and_then(|above| {
            self.children
                .get(&number(above))?
                .iter()
                .find_map(|(n, i)| {
                    let r = self.records.get(n)?;
                    (r.names.get(*i)?.name == name).then_some(r.reference)
                })
        });
        found.insert(rel.to_path_buf(), reference);
        reference
    }

    /// What `windows::verdict` says of the stored folder at `rel`, its
    /// record `reference` where the table found one.
    fn verdict(
        &self,
        points: &[(PathBuf, bool)],
        rel: &Path,
        stored: crate::StoredFolder,
        reference: Option<Reference>,
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
        let Some(record) = reference.and_then(|r| self.get(r)) else {
            return Some(Stale::Gone);
        };
        if !record.folder {
            return Some(Stale::Replaced);
        }
        match folder_kind(record.attributes, record.tag, || record.volume) {
            Folder::Entered => {}
            Folder::Link => return Some(Stale::Replaced),
            Folder::NotCovered(Why::NetworkMount) => return Some(Stale::NetworkMount),
            Folder::NotCovered(_) => return Some(Stale::Mount),
        }
        let stat = Table::stat_of(record, self.serial);
        if stat.id != stored.id {
            return Some(Stale::Replaced);
        }
        (stat.ctime != stored.ctime).then_some(Stale::Changed)
    }

    /// A folder's own facts, as its stat gives them, on the volume `dev`
    /// names as a stat does.
    fn stat_of(record: &Record, dev: u64) -> FolderStat {
        FolderStat {
            id: FileId {
                dev,
                ino: record.reference,
            },
            mount: dev,
            ctime: record.ctime,
            mtime: record.mtime,
            owner: 0,
            group: 0,
            mode: 0,
        }
    }
}

/// Folders read from a volume's file table for a crawl
/// ([`crate::crawl_by`]), as `windows::read` reads them from their own
/// listings: the same exclusions, mounts and links, a listing built by the
/// same `listed`. A folder is found by the identity its parent's listing
/// gave it, the root by its own.
///
/// What the table could not read ([`Table::unread`]) would be in no
/// listing, its folder called whole without it: a table holding such a
/// record is not read so (the engine walks the root's folders instead).
/// The journal says nothing of a record NTFS marked bad, nor of one the
/// table cannot decode.
pub struct TableReader<'a> {
    pub table: &'a Table,
}

impl TableReader<'_> {
    /// The folder at `rel` below the root, its identity `id` where its
    /// parent's listing gave it.
    pub fn read(
        &self,
        root: &Root,
        rel: &Path,
        id: Option<FileId>,
        opts: &Options,
    ) -> (Option<FolderStat>, Outcome, Watch) {
        let (stat, outcome) = self.folder(root, rel, id, opts);
        (stat, outcome, Watch::Unasked)
    }

    fn folder(
        &self,
        root: &Root,
        rel: &Path,
        id: Option<FileId>,
        opts: &Options,
    ) -> (Option<FolderStat>, Outcome) {
        let not_covered = |stat, why| (stat, Outcome::NotCovered(why));
        if root.excluded_by_path(rel) {
            return not_covered(None, Why::Excluded);
        }
        if let Some(network) = root.mount_at(rel) {
            return not_covered(None, mount_why(network));
        }
        let id = match id {
            Some(id) => id,
            None if rel.as_os_str().is_empty() => root.stat.id,
            None => return not_covered(None, Why::Gone),
        };
        // The volume as a stat names it (`FILE_ID_INFO`), which the walk's
        // identities carry: on NTFS, the same 64 bits as the table's serial
        // (`FSCTL_GET_NTFS_VOLUME_DATA`), seen so on Windows Server 2025. A
        // root on another volume than the table's has none of its folders.
        let dev = root.stat.id.dev;
        if dev != self.table.serial || id.dev != dev {
            return not_covered(None, Why::Mount);
        }
        let Some(record) = self.table.get(id.ino) else {
            return not_covered(None, Why::Gone);
        };
        let mut stat = Table::stat_of(record, dev);
        if opts.acls {
            // The system service's: what an asker is judged by (DESIGN 4).
            stat.owner = record.security.unwrap_or(0);
            root.learn(stat.owner, record.reference);
        }
        if !record.folder {
            return not_covered(Some(stat), Why::Replaced);
        }
        match folder_kind(record.attributes, record.tag, || record.volume) {
            Folder::Entered => {}
            Folder::Link => return not_covered(Some(stat), Why::Replaced),
            Folder::NotCovered(why) => {
                root.found_mount(rel.to_path_buf(), why == Why::NetworkMount);
                return not_covered(Some(stat), why);
            }
        }
        if root.exclusions.holds(Some(stat.id), &root.path.join(rel)) {
            return not_covered(Some(stat), Why::Excluded);
        }
        let items = self.table.items(record.reference);
        let outcome = listed(
            root,
            rel,
            stat,
            self.table.read_at,
            items,
            opts,
            None,
            &|item| self.table.get(item.id).is_some_and(|r| r.volume),
        );
        (Some(stat), outcome)
    }
}

/// Where a volume keeps its file table, and how it is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub serial: u64,
    pub bytes_per_sector: u64,
    pub bytes_per_cluster: u64,
    pub bytes_per_record: u64,
    /// The table's first cluster.
    pub mft_start: u64,
    /// The bytes of the table in use.
    pub mft_valid: u64,
}

/// What is read of the table at once.
const CHUNK: usize = 1 << 20;

impl Volume {
    /// Where its file table is: an error on a volume that is not NTFS.
    pub fn layout(&self) -> std::io::Result<Layout> {
        let mut out = [0u64; size_of::<NTFS_VOLUME_DATA_BUFFER>().div_ceil(8)];
        control(&self.0, FSCTL_GET_NTFS_VOLUME_DATA, &(), &mut out)?;
        // SAFETY: the call wrote the structure at the buffer's start,
        // aligned for it; made of integers, any bytes are one.
        let data = unsafe { &*out.as_ptr().cast::<NTFS_VOLUME_DATA_BUFFER>() };
        let unsigned = |v: i64| u64::try_from(v).map_err(|_| invalid("a negative size"));
        Ok(Layout {
            serial: u64::from_le_bytes(data.VolumeSerialNumber.to_le_bytes()),
            bytes_per_sector: u64::from(data.BytesPerSector),
            bytes_per_cluster: u64::from(data.BytesPerCluster),
            bytes_per_record: u64::from(data.BytesPerFileRecordSegment),
            mft_start: unsigned(data.MftStartLcn)?,
            mft_valid: unsigned(data.MftValidDataLength)?,
        })
    }

    /// `buffer.len()` bytes of the volume from `offset`, both whole
    /// sectors, as a volume is read.
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        let mut overlapped = OVERLAPPED::default();
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        let len = u32::try_from(buffer.len()).map_err(|_| invalid("a read too long"))?;
        let mut read = 0u32;
        // SAFETY: the buffer and its length, alive for the call; the
        // handle is synchronous, so the read is done when it returns, from
        // the offset the structure gives.
        let ok = unsafe {
            ReadFile(
                self.0.0,
                buffer.as_mut_ptr(),
                len,
                &raw mut read,
                &raw mut overlapped,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if read != len {
            return Err(invalid("the volume ended before its file table"));
        }
        Ok(())
    }

    /// The file table of the volume holding `path`, read whole from the
    /// disk as of now: the volume's buffers written to it first
    /// (`FlushFileBuffers`, which an administrator may), its table among
    /// them, and the table read past every cache (`FILE_FLAG_NO_BUFFERING`).
    /// A raw read sees only what is on the disk. Unflushed, a file made a
    /// moment before the root was followed would be in neither the table
    /// nor the journal heard from then, and the root said current without
    /// it (rule 1): the runner's tests read stale tables until they
    /// flushed. Refused where the volume cannot be flushed: its folders are
    /// walked instead.
    pub fn table_of(path: &Path) -> std::io::Result<Table> {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FlushFileBuffers};
        let device = super::journal::device_of(path)?;
        let writable = super::journal::open_device_as(&device, GENERIC_READ | GENERIC_WRITE, 0)?;
        // SAFETY: the volume's handle, opened to write; every file of it
        // flushed, waited for.
        if unsafe { FlushFileBuffers(writable.0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        drop(writable);
        // Whole sectors at sector offsets, into page-aligned memory
        // ([`Pages`]), as a read past the cache must be.
        Volume(super::journal::open_device(
            &device,
            FILE_FLAG_NO_BUFFERING,
        )?)
        .table()
    }

    /// Its file table, read whole from the disk, as the handle reads it.
    fn table(&self) -> std::io::Result<Table> {
        let read_at = Time::now();
        let layout = self.layout()?;
        let record =
            usize::try_from(layout.bytes_per_record).map_err(|_| invalid("a record too large"))?;
        let sector =
            usize::try_from(layout.bytes_per_sector).map_err(|_| invalid("a sector too large"))?;
        let cluster = layout.bytes_per_cluster;
        if record < STRIDE || record % STRIDE != 0 || sector == 0 || CHUNK % record != 0 {
            return Err(invalid("a record size NTFS does not make"));
        }
        // The table's first record: where the rest is.
        let mut first = Pages::new(record.max(sector));
        let head = &mut first.bytes_mut()[..record.next_multiple_of(sector)];
        self.read_at(layout.mft_start * cluster, head)?;
        let itself = decode(0, &mut head[..record])
            .map_err(|why| invalid(&format!("its first record: {why:?}")))?;
        if itself.listed {
            // Its runs go on in other records, found by reading it: kept
            // for when a volume needs it, a walk meanwhile.
            return Err(invalid("a file table too fragmented for its first record"));
        }
        let runs = itself
            .runs
            .ok_or_else(|| invalid("a file table with no data"))?;
        let total = layout.mft_valid / layout.bytes_per_record;
        let mut chunk = Pages::new(CHUNK);
        let mut read = read_runs(
            &runs,
            cluster,
            record,
            total,
            chunk.bytes_mut(),
            &mut |at, bytes| self.read_at(at, bytes),
        )?;
        // A record torn as it was read, being written, is whole once the
        // write is done: each read once again, the rest of the table read.
        let mut unread = Vec::new();
        for (n, at) in std::mem::take(&mut read.unread) {
            let again = match at {
                Some(at) => self
                    .record_at(at, record, sector)
                    .map(|mut r| decode(n, &mut r)),
                None => Ok(Err(Bad::Torn)),
            };
            match again? {
                Ok(d) => read.decoded.push(d),
                Err(Bad::Empty | Bad::Unused) => {}
                Err(Bad::Torn | Bad::Malformed) => unread.push(n),
            }
        }
        let mut table = Table::of(read.decoded, unread);
        table.read_at = read_at;
        table.serial = layout.serial;
        Ok(table)
    }

    /// The `record` bytes of the volume at `at`, read alone: the sectors
    /// holding them, as a volume is read past its cache.
    fn record_at(&self, at: u64, record: usize, sector: usize) -> std::io::Result<Vec<u8>> {
        let sector = sector as u64;
        let start = at - at % sector;
        let skip = usize::try_from(at - start).map_err(|_| invalid("a sector too large"))?;
        let len = (skip + record).next_multiple_of(sector as usize);
        let mut pages = Pages::new(len);
        let bytes = &mut pages.bytes_mut()[..len];
        self.read_at(start, bytes)?;
        Ok(bytes[skip..skip + record].to_vec())
    }
}

/// What the table's runs hold ([`read_runs`]).
#[derive(Debug, Default)]
pub(crate) struct Runs {
    pub decoded: Vec<Decoded>,
    /// The records not read (torn, malformed), each with where it is on the
    /// volume, to be read again; none for one split between two runs.
    pub unread: Vec<(u64, Option<u64>)>,
}

/// The `total` records the table's `runs` hold, read through `read` (an
/// offset on the volume, whole sectors) a `chunk` at a time, each decoded
/// under its number, counted from the table's start. A run need not end
/// on a record's end (512-byte clusters, 1 KiB records): a record split
/// between two runs is put back together, and every record after it keeps
/// its number.
pub(crate) fn read_runs(
    runs: &[Run],
    cluster: u64,
    record: usize,
    total: u64,
    chunk: &mut [u8],
    read: &mut dyn FnMut(u64, &mut [u8]) -> std::io::Result<()>,
) -> std::io::Result<Runs> {
    fn decoded(n: u64, bytes: &mut [u8], at: Option<u64>, out: &mut Runs) {
        match decode(n, bytes) {
            Ok(d) => out.decoded.push(d),
            Err(Bad::Empty | Bad::Unused) => {}
            Err(Bad::Torn | Bad::Malformed) => out.unread.push((n, at)),
        }
    }
    let mut out = Runs::default();
    // The start of a record the chunk before ended within, and where it
    // began on the volume while it goes on there (a chunk of the same
    // run), none once it goes on elsewhere (the next run).
    let mut split: Vec<u8> = Vec::with_capacity(record);
    let mut split_at: Option<u64> = None;
    let mut next: u64 = 0;
    for run in runs {
        let lcn = run
            .lcn
            .ok_or_else(|| invalid("a sparse run in the file table"))?;
        let bounds = lcn.checked_mul(cluster).zip(
            lcn.checked_add(run.clusters)
                .and_then(|e| e.checked_mul(cluster)),
        );
        let (mut at, end) = bounds.ok_or_else(|| invalid("a run past the volume's end"))?;
        while at < end && next < total {
            let len = usize::try_from((end - at).min(chunk.len() as u64)).unwrap_or(chunk.len());
            let bytes = &mut chunk[..len];
            read(at, bytes)?;
            let mut from = 0;
            if !split.is_empty() {
                if split_at.is_some_and(|s| s + split.len() as u64 != at) {
                    split_at = None;
                }
                from = (record - split.len()).min(len);
                split.extend_from_slice(&bytes[..from]);
                if split.len() == record {
                    decoded(next, &mut split, split_at, &mut out);
                    next += 1;
                    split.clear();
                }
            }
            let rest = &mut bytes[from..];
            let whole = rest.len() / record * record;
            for (k, one) in rest[..whole].chunks_exact_mut(record).enumerate() {
                if next >= total {
                    break;
                }
                let offset = at + (from + k * record) as u64;
                decoded(next, one, Some(offset), &mut out);
                next += 1;
            }
            if next < total && whole < rest.len() {
                split_at = Some(at + (from + whole) as u64);
                split.extend_from_slice(&rest[whole..]);
            }
            at += len as u64;
        }
    }
    Ok(out)
}

fn invalid(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned())
}

/// Memory aligned on pages, as a volume's reads may need: a read that
/// bypasses the cache takes a buffer aligned as the disk asks.
struct Pages(Vec<Page>);

#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct Page([u8; 4096]);

impl Pages {
    fn new(bytes: usize) -> Pages {
        Pages(vec![Page([0; 4096]); bytes.div_ceil(4096)])
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: pages are plain bytes, contiguous in the vector, each
        // initialized; the slice borrows the vector mutably.
        unsafe {
            std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast::<u8>(), self.0.len() * 4096)
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests;
