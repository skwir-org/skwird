//! An index saved to a folder of its own, and read back mapped
//! (`docs/DESIGN.md`, decision 8).
//!
//! The folder holds a manifest, the chunks, each in a file named by its
//! number, and the names, in segments: what the index was built with,
//! then what each save added. A file is written once and never again: a
//! save writes what is new under a temporary name, makes it durable,
//! renames it, and then switches the manifest the same way, so that a
//! save cut short leaves the last manifest and every file it names. What
//! no manifest names is removed after. The folder is its user's alone
//! (mode 0700), and so is each file (0600): an index is a list of what is
//! on its user's disk.
//!
//! The index is a cache: a manifest of another version, or a file that
//! does not read as its layout, is refused whole, and the index rebuilt.
//!
//! The manifest, little-endian: `SKWM`, its version, the chunks' and the
//! names' layout versions (4 bytes each); the save's number (8); when it
//! was saved, when the index was last crawled whole, and when its names
//! were last proven (8 and 4 each); the log's epoch, and the number of
//! the last record the chunks hold (8 each); where the change history was
//! read up to, if it was ([`Position`]: 1, then 8 and 8, or 1 alone; from
//! version 4, version 3 read as none); the number the next names segment
//! takes (8); the number of names
//! segments (4) and each one's number (8 each); the number of chunks (4)
//! and each one's number (4 each).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use skwird_crawl::Time;

use crate::block::Item;
use crate::chunk::{self, Bytes, Chunk};
use crate::index::Index;
use crate::map::Map;
use crate::names::{self, Names, Segment};
use crate::tree::{ChunkId, Limits, Tree};

const MAGIC: &[u8; 4] = b"SKWM";
const VERSION: u32 = 4;
/// The manifest's version before the position was kept, still read.
const WITHOUT_POSITION: u32 = 3;
const MANIFEST: &str = "manifest";

/// Why an index could not be read back.
#[derive(Debug)]
pub enum LoadError {
    /// No index was saved there.
    Missing,
    /// It was saved by another layout of the index.
    Layout,
    /// Its files do not read as they were written.
    Damaged,
    Io(std::io::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Missing => write!(f, "no index is saved there"),
            LoadError::Layout => write!(f, "the index was saved in another layout"),
            LoadError::Damaged => write!(f, "the index's files are damaged"),
            LoadError::Io(e) => write!(f, "the index cannot be read: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// What a manifest says.
struct Manifest {
    generation: u64,
    mark: Mark,
    next_segment: u64,
    segments: Vec<u64>,
    chunks: Vec<ChunkId>,
}

/// What a save says of the index, and what it reads back as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub saved_at: Time,
    /// When it was last crawled whole: its attributes are as of then.
    pub crawled_at: Time,
    /// When its names were last proven: what it was saved with is as of
    /// then.
    pub proven_at: Time,
    /// The log's epoch, and the number of the last record of it the chunks
    /// hold: the records after it are to be replayed.
    pub epoch: u64,
    pub log_seq: u64,
    /// Where the change history was read up to, every change before it
    /// applied in what is saved: none where none was, or what was read
    /// since is not proven ([`Position`]).
    pub position: Option<Position>,
}

/// A position in a system's history of changes, for an index to resume
/// from (DESIGN 2): the history's identity, and the next record to read
/// from it. On Windows, the change journal's id and its next USN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub history: u64,
    pub next: i64,
}

fn chunk_file(id: ChunkId) -> String {
    format!("chunk-{id:010}")
}

fn names_file(id: u64) -> String {
    format!("names-{id:020}")
}

/// Reads a manifest's fields in turn.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let part = self.bytes.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(part)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
    fn time(&mut self) -> Option<Time> {
        Some(Time {
            sec: self.take().map(i64::from_le_bytes)?,
            nsec: self.u32()?,
        })
    }
}

fn read_manifest(dir: &Path) -> Result<Manifest, LoadError> {
    let bytes = match std::fs::read(dir.join(MANIFEST)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::Missing),
        Err(e) => return Err(LoadError::Io(e)),
    };
    let mut r = Reader {
        bytes: &bytes,
        at: 0,
    };
    if r.take::<4>().as_ref() != Some(MAGIC) {
        return Err(LoadError::Damaged);
    }
    let version = r.u32();
    if !matches!(version, Some(VERSION | WITHOUT_POSITION))
        || r.u32() != Some(chunk::VERSION)
        || r.u32() != Some(names::VERSION)
    {
        return Err(LoadError::Layout);
    }
    let mut parse = || -> Option<Manifest> {
        let generation = r.u64()?;
        let mark = Mark {
            saved_at: r.time()?,
            crawled_at: r.time()?,
            proven_at: r.time()?,
            epoch: r.u64()?,
            log_seq: r.u64()?,
            position: None,
        };
        let mark = if version == Some(VERSION) {
            Mark {
                position: match r.take::<1>()? {
                    [0] => None,
                    [1] => Some(Position {
                        history: r.u64()?,
                        next: r.take().map(i64::from_le_bytes)?,
                    }),
                    _ => return None,
                },
                ..mark
            }
        } else {
            mark
        };
        let next_segment = r.u64()?;
        let count = r.u32()? as usize;
        let segments = (0..count).map(|_| r.u64()).collect::<Option<Vec<_>>>()?;
        let count = r.u32()? as usize;
        let chunks = (0..count).map(|_| r.u32()).collect::<Option<Vec<_>>>()?;
        (r.at == bytes.len() && !segments.is_empty()).then_some(Manifest {
            generation,
            mark,
            next_segment,
            segments,
            chunks,
        })
    };
    parse().ok_or(LoadError::Damaged)
}

/// Write `bytes` to `dir/name` durably, through a temporary name, its
/// user's alone.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, dir.join(name))
}

/// Make the folder's own entries durable: the renames done in it.
// Elsewhere than Unix a folder is not opened to be synced, and this
// always succeeds; its callers are the same on every system.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// The index's folder, made its user's alone if it is not there.
pub(crate) fn make_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Save `index` in `dir`, with what `mark` says of it: what is new in it written (chunks in memory, names
/// given since), the manifest switched, what no manifest names removed.
/// What was written is mapped back in place of what was in memory, so its
/// pages are the system's to drop. A save that fails leaves the index as
/// it was, and the last manifest as it was.
pub fn save(index: &mut Index, dir: &Path, mark: &Mark) -> std::io::Result<()> {
    make_dir(dir)?;
    let previous = read_manifest(dir).ok();
    let generation = previous.as_ref().map_or(0, |m| m.generation + 1);
    let mut next_segment = previous.as_ref().map_or(0, |m| m.next_segment);
    let mut segments = Vec::new();
    // Whether a file of names is written: only then are they read from
    // their files again.
    let mut wrote_names = false;
    for segment in index.names().segments() {
        match segment.file() {
            Some(id) if segment.is_mapped() && dir.join(names_file(id)).is_file() => {
                segments.push(id);
            }
            _ => {
                write_file(dir, &names_file(next_segment), segment.bytes())?;
                segments.push(next_segment);
                next_segment += 1;
                wrote_names = true;
            }
        }
    }
    if let Some(bytes) = index.names().pending_bytes() {
        write_file(dir, &names_file(next_segment), &bytes)?;
        segments.push(next_segment);
        next_segment += 1;
        wrote_names = true;
    }
    // A chunk changed in memory under a number a file already has (patched
    // or spliced since it was saved) takes another first: no file a saved
    // manifest names is written over, and a kill before the manifest is
    // switched leaves it whole. Renumbered here, once a save, not at each
    // change (DESIGN 8).
    let stale: Vec<ChunkId> = index
        .tree()
        .chunks()
        .filter(|(id, chunk)| !chunk.is_mapped() && dir.join(chunk_file(*id)).is_file())
        .map(|(id, _)| id)
        .collect();
    for id in stale {
        index.tree_mut().renumber(id);
    }
    let mut written = Vec::new();
    for (id, chunk) in index.tree().chunks() {
        // A chunk read from this folder's file is there already.
        if !(chunk.is_mapped() && dir.join(chunk_file(id)).is_file()) {
            write_file(dir, &chunk_file(id), chunk.bytes())?;
            written.push(id);
        }
    }
    let ids = index.tree().chunk_ids().to_vec();
    let mut m = Vec::with_capacity(64 + segments.len() * 8 + ids.len() * 4);
    m.extend_from_slice(MAGIC);
    for v in [VERSION, chunk::VERSION, names::VERSION] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m.extend_from_slice(&generation.to_le_bytes());
    for t in [mark.saved_at, mark.crawled_at, mark.proven_at] {
        m.extend_from_slice(&t.sec.to_le_bytes());
        m.extend_from_slice(&t.nsec.to_le_bytes());
    }
    m.extend_from_slice(&mark.epoch.to_le_bytes());
    m.extend_from_slice(&mark.log_seq.to_le_bytes());
    match mark.position {
        None => m.push(0),
        Some(p) => {
            m.push(1);
            m.extend_from_slice(&p.history.to_le_bytes());
            m.extend_from_slice(&p.next.to_le_bytes());
        }
    }
    m.extend_from_slice(&next_segment.to_le_bytes());
    m.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    for s in &segments {
        m.extend_from_slice(&s.to_le_bytes());
    }
    m.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in &ids {
        m.extend_from_slice(&id.to_le_bytes());
    }
    // What was written is named in the folder before the manifest names it.
    if wrote_names || !written.is_empty() {
        sync_dir(dir)?;
    }
    write_file(dir, MANIFEST, &m)?;
    sync_dir(dir)?;
    // Saved: what is written is read from its file from now on.
    for id in written {
        if let Ok(chunk) = open(dir, &chunk_file(id))
            .ok()
            .and_then(|b| Chunk::from_bytes(b).ok())
            .ok_or(())
        {
            index.tree_mut().set_chunk(id, Arc::new(chunk));
        }
    }
    // Read from their files again only where one was written: those
    // already read from theirs are as they were.
    let mapped = || {
        segments
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                let bytes = open(dir, &names_file(id)).ok()?;
                Segment::from_bytes(bytes, i == 0, Some(id)).ok()
            })
            .collect::<Option<Vec<_>>>()
    };
    if wrote_names && let Some(mapped) = mapped() {
        index.names_mut().sealed(mapped);
    }
    remove_unnamed(dir, &segments, &ids);
    Ok(())
}

fn open(dir: &Path, name: &str) -> Result<Bytes, LoadError> {
    Map::open(&dir.join(name)).map(Bytes::Mapped).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            LoadError::Damaged
        } else {
            LoadError::Io(e)
        }
    })
}

/// Remove what the manifest does not name: older chunks and names, and
/// what a save cut short left.
fn remove_unnamed(dir: &Path, segments: &[u64], ids: &[ChunkId]) {
    let keep: HashSet<String> = ids
        .iter()
        .map(|&id| chunk_file(id))
        .chain(segments.iter().map(|&s| names_file(s)))
        .chain([MANIFEST.to_owned()])
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let tmp = Path::new(&name).extension().is_some_and(|e| e == "tmp");
        let ours = name.starts_with("chunk-") || name.starts_with("names-") || tmp;
        if ours && !keep.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The index saved in `dir`, its files mapped, and what it says of
/// itself. Every file is checked as it is read; any that does not read
/// as its layout refuses the whole.
/// What the last save in `dir` says, its manifest alone read: where its
/// change history was read up to, before the index is loaded. None where
/// none is saved, or it does not read.
pub fn read_mark(dir: &Path) -> Option<Mark> {
    read_manifest(dir).ok().map(|m| m.mark)
}

pub fn load(dir: &Path, limits: Limits) -> Result<(Index, Mark), LoadError> {
    let manifest = read_manifest(dir)?;
    let segments = manifest
        .segments
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            Segment::from_bytes(open(dir, &names_file(id))?, i == 0, Some(id))
                .map_err(|_| LoadError::Damaged)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let names = Names::from_segments(segments);
    let chunks = manifest
        .chunks
        .iter()
        .map(|&id| {
            let chunk =
                Chunk::from_bytes(open(dir, &chunk_file(id))?).map_err(|_| LoadError::Damaged)?;
            Ok((id, Arc::new(chunk)))
        })
        .collect::<Result<Vec<_>, LoadError>>()?;
    let tree = Tree::from_chunks(chunks, limits).map_err(|_| LoadError::Damaged)?;
    check_entries(&tree, &names)?;
    Ok((Index::new(tree, names), manifest.mark))
}

/// Whether `index` is sound: its blocks one tree in pre-order (each
/// folder once, each below its parent), every name it names in its names,
/// and every folder entry naming a folder whose block says it is that
/// entry. A load checks as much; a test checks it after every change.
pub fn check(index: &Index) -> Result<(), LoadError> {
    let chunks: Vec<_> = index
        .tree()
        .chunks()
        .map(|(id, c)| (id, Arc::clone(c)))
        .collect();
    let tree = Tree::from_chunks(chunks, Limits::default()).map_err(|_| LoadError::Damaged)?;
    check_entries(&tree, index.names())
}

/// Every name an index names is in its names, and every folder entry
/// names a folder whose block says it is that entry.
fn check_entries(tree: &Tree, names: &Names) -> Result<(), LoadError> {
    let count = names.len() as u64;
    for (_, chunk) in tree.chunks() {
        let view = chunk.view();
        for b in 0..chunk.len() {
            if u64::from(view.name(b)) >= count {
                return Err(LoadError::Damaged);
            }
            let folder = view.folder(b);
            for i in view.entries(b) {
                let Item { name, child, .. } = view.item(i);
                if u64::from(name) >= count {
                    return Err(LoadError::Damaged);
                }
                if let Some(c) = child {
                    let Some(loc) = tree.loc(c) else {
                        return Err(LoadError::Damaged);
                    };
                    let theirs = tree.chunk(loc.chunk).view();
                    if theirs.parent(loc.block as usize) != Some(folder)
                        || theirs.name(loc.block as usize) != name
                    {
                        return Err(LoadError::Damaged);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Where the index of a root is kept below a cache folder.
pub fn dir_for(cache: &Path, key: &str) -> PathBuf {
    cache.join("skwird").join(key)
}
