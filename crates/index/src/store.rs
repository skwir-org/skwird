//! An index saved to a folder of its own, and read back mapped
//! (`docs/DESIGN.md`, decision 8).
//!
//! The folder holds a manifest, the chunks, each in a file named by its
//! number, and the names. A file is written once and never again: a save
//! writes what is new under a temporary name, makes it durable, renames
//! it, and then switches the manifest the same way, so that a save cut
//! short leaves the last manifest and every file it names. What no
//! manifest names is removed after. The folder is its user's alone
//! (mode 0700), and so is each file (0600): an index is a list of what is
//! on its user's disk.
//!
//! The index is a cache: a manifest of another version, or a file that
//! does not read as its layout, is refused whole, and the index rebuilt.
//!
//! The manifest, little-endian: `SKWM`, its version, the chunks' and the
//! names' layout versions (4 bytes each), the save's number (8), when it
//! was saved (8 and 4), the number of the save whose names it takes (8),
//! the number of chunks (4), and each chunk's number (4 each).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use skwird_crawl::Time;

use crate::block::Item;
use crate::chunk::{self, Bytes, Chunk};
use crate::index::Index;
use crate::map::Map;
use crate::names::{self, Names};
use crate::tree::{ChunkId, Limits, Tree};

const MAGIC: &[u8; 4] = b"SKWM";
const VERSION: u32 = 1;
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
    saved_at: Time,
    names: u64,
    chunks: Vec<ChunkId>,
}

fn chunk_file(id: ChunkId) -> String {
    format!("chunk-{id:010}")
}

fn names_file(generation: u64) -> String {
    format!("names-{generation:020}")
}

fn read_manifest(dir: &Path) -> Result<Manifest, LoadError> {
    let bytes = match std::fs::read(dir.join(MANIFEST)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::Missing),
        Err(e) => return Err(LoadError::Io(e)),
    };
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    if bytes.get(..4) != Some(MAGIC.as_slice()) {
        return Err(LoadError::Damaged);
    }
    if u32_at(4) != Some(VERSION)
        || u32_at(8) != Some(chunk::VERSION)
        || u32_at(12) != Some(names::VERSION)
    {
        return Err(LoadError::Layout);
    }
    let parse = || -> Option<Manifest> {
        let generation = u64_at(16)?;
        let saved_at = Time {
            sec: i64::from_le_bytes(bytes.get(24..32)?.try_into().ok()?),
            nsec: u32_at(32)?,
        };
        let names = u64_at(36)?;
        let count = u32_at(44)? as usize;
        let chunks: Vec<ChunkId> = (0..count)
            .map(|i| u32_at(48 + i * 4))
            .collect::<Option<_>>()?;
        (bytes.len() == 48 + count * 4).then_some(Manifest {
            generation,
            saved_at,
            names,
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
fn make_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Save `index` in `dir`, as of `saved_at`: what is new in it written, the
/// manifest switched, what no manifest names removed. What was written is
/// mapped back in place of what was in memory, so its pages are the
/// system's to drop. A save that fails leaves the index as it was, and
/// the last manifest as it was.
pub fn save(index: &mut Index, dir: &Path, saved_at: Time) -> std::io::Result<()> {
    make_dir(dir)?;
    let previous = read_manifest(dir).ok();
    let generation = previous.as_ref().map_or(0, |m| m.generation + 1);
    let names_generation = match &previous {
        Some(m) if index.names_mapped_from(dir) => m.names,
        _ => {
            write_file(dir, &names_file(generation), index.names().bytes())?;
            generation
        }
    };
    let mut written = Vec::new();
    for (id, chunk) in index.tree().chunks() {
        // A chunk read from this folder's file is there already.
        if !(chunk.is_mapped() && dir.join(chunk_file(id)).is_file()) {
            write_file(dir, &chunk_file(id), chunk.bytes())?;
            written.push(id);
        }
    }
    let ids = index.tree().chunk_ids().to_vec();
    let mut manifest = Vec::with_capacity(48 + ids.len() * 4);
    manifest.extend_from_slice(MAGIC);
    manifest.extend_from_slice(&VERSION.to_le_bytes());
    manifest.extend_from_slice(&chunk::VERSION.to_le_bytes());
    manifest.extend_from_slice(&names::VERSION.to_le_bytes());
    manifest.extend_from_slice(&generation.to_le_bytes());
    manifest.extend_from_slice(&saved_at.sec.to_le_bytes());
    manifest.extend_from_slice(&saved_at.nsec.to_le_bytes());
    manifest.extend_from_slice(&names_generation.to_le_bytes());
    manifest.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in &ids {
        manifest.extend_from_slice(&id.to_le_bytes());
    }
    sync_dir(dir)?;
    write_file(dir, MANIFEST, &manifest)?;
    sync_dir(dir)?;
    // Saved: what is written is read from its file from now on.
    for id in written {
        let path = dir.join(chunk_file(id));
        if let Ok(chunk) = Map::open(&path).map(Bytes::Mapped).and_then(|b| {
            Chunk::from_bytes(b).map_err(|_| std::io::Error::other("a chunk just written"))
        }) {
            index.tree_mut().set_chunk(id, Arc::new(chunk));
        }
    }
    if names_generation == generation
        && let Ok(names) = Map::open(&dir.join(names_file(generation)))
            .map(Bytes::Mapped)
            .map_err(|_| ())
            .and_then(|b| Names::from_bytes(b).map_err(|_| ()))
    {
        index.set_names(Arc::new(names), dir);
    }
    remove_unnamed(dir, names_generation, &ids);
    Ok(())
}

/// Remove what the manifest does not name: older chunks and names, and
/// what a save cut short left.
fn remove_unnamed(dir: &Path, names_generation: u64, ids: &[ChunkId]) {
    let keep: HashSet<String> = ids
        .iter()
        .map(|&id| chunk_file(id))
        .chain([names_file(names_generation), MANIFEST.to_owned()])
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

/// The index saved in `dir`, its files mapped, and when it was saved.
/// Every file is checked as it is read; any that does not read as its
/// layout refuses the whole.
pub fn load(dir: &Path, limits: Limits) -> Result<(Index, Time), LoadError> {
    let manifest = read_manifest(dir)?;
    let open = |name: String| -> Result<Bytes, LoadError> {
        Map::open(&dir.join(name)).map(Bytes::Mapped).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                LoadError::Damaged
            } else {
                LoadError::Io(e)
            }
        })
    };
    let names =
        Names::from_bytes(open(names_file(manifest.names))?).map_err(|_| LoadError::Damaged)?;
    let chunks = manifest
        .chunks
        .iter()
        .map(|&id| {
            let chunk = Chunk::from_bytes(open(chunk_file(id))?).map_err(|_| LoadError::Damaged)?;
            Ok((id, Arc::new(chunk)))
        })
        .collect::<Result<Vec<_>, LoadError>>()?;
    let tree = Tree::from_chunks(chunks, limits).map_err(|_| LoadError::Damaged)?;
    check_entries(&tree, &names)?;
    let mut index = Index::new(tree, Arc::new(names));
    index.mark_names_from(dir);
    Ok((index, manifest.saved_at))
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
