//! A file mapped into memory and read in place (`docs/DESIGN.md`,
//! decision 8): its pages are the system's to drop under pressure and
//! read again, so a large index weighs on memory only where it is read.
//!
//! The one place the index speaks to the system with `unsafe`. A mapped
//! file must not change while it is mapped, or what was read as one
//! slice of bytes would change under it. The index never writes a file
//! it has written once: a chunk rewritten is a new file, and each file is
//! its user's alone (mode 0600, in a folder of mode 0700).
//!
//! - **Unix:** `mmap`, read-only and private. A file the manifest no
//!   longer names is removed at the save that let it go, mapped or not:
//!   its pages stay the mapping's until it is unmapped.
//! - **Windows** (since 0.2): a view of a read-only file mapping
//!   (`CreateFileMappingW`, `MapViewOfFile`), the file opened to share
//!   reading and removal. Read into memory, the index was the engine's
//!   own: 39.6 to 48.9 MB for 1.04 million entries on the runners
//!   (2026-11-24), over the budget of 10 MB per million for memory at
//!   rest (DESIGN 10). Windows may refuse to remove a file while a view
//!   of it is alive (an older snapshot's): every save removes what its
//!   manifest does not name, so such a file goes at a save after the
//!   snapshot is let go of.
//! - Elsewhere, the file is read into memory.

#![allow(unsafe_code)]

use std::ops::Deref;
use std::path::Path;

#[cfg(unix)]
pub struct Map {
    ptr: *mut std::ffi::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and private, never written through,
// and unmapped only when dropped: sharing it between threads shares bytes
// that do not change.
#[cfg(unix)]
unsafe impl Send for Map {}
// SAFETY: as above.
#[cfg(unix)]
unsafe impl Sync for Map {}

#[cfg(unix)]
impl Map {
    /// The file at `path`, mapped read-only.
    pub fn open(path: &Path) -> std::io::Result<Map> {
        use rustix::mm::{MapFlags, ProtFlags};
        let file = std::fs::File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| std::io::Error::other("a file too large to map"))?;
        if len == 0 {
            return Ok(Map {
                ptr: std::ptr::null_mut(),
                len: 0,
            });
        }
        // SAFETY: a new mapping, at an address the system chooses, of a
        // file open for reading; nothing else refers to that address. The
        // file is one the index wrote once and never writes again (the
        // module's doc).
        let ptr = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                &file,
                0,
            )
        }?;
        Ok(Map { ptr, len })
    }
}

#[cfg(unix)]
impl Deref for Map {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `ptr` is a live mapping of `len` readable bytes until
        // `drop`, and the file behind it is never written (the module's
        // doc).
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

#[cfg(unix)]
impl Drop for Map {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: the mapping made in `open`, of that length, unmapped
            // once: no slice of it outlives `self`, which `deref` borrows.
            let _ = unsafe { rustix::mm::munmap(self.ptr, self.len) };
        }
    }
}

#[cfg(windows)]
pub struct Map {
    view: *mut std::ffi::c_void,
    len: usize,
}

// SAFETY: the view is read-only, never written through, and unmapped only
// when dropped: sharing it between threads shares bytes that do not
// change.
#[cfg(windows)]
unsafe impl Send for Map {}
// SAFETY: as above.
#[cfg(windows)]
unsafe impl Sync for Map {}

#[cfg(windows)]
impl Map {
    /// The file at `path`, a read-only view of it.
    pub fn open(path: &Path) -> std::io::Result<Map> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY,
        };
        // Its removal shared, so that a save may remove it once no view is
        // left, and try while one is.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| std::io::Error::other("a file too large to map"))?;
        if len == 0 {
            // A mapping of nothing is refused: nothing to view.
            return Ok(Map {
                view: std::ptr::null_mut(),
                len: 0,
            });
        }
        // SAFETY: a handle of ours open for reading, no attributes, the
        // file's own size, no name: a new mapping, ours to close.
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the mapping just made, viewed whole for reading; the view
        // keeps the mapping and the file alive once both handles are
        // closed.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        let error = std::io::Error::last_os_error();
        // SAFETY: the mapping's handle, closed once; the view holds it.
        unsafe { CloseHandle(mapping) };
        if view.Value.is_null() {
            return Err(error);
        }
        Ok(Map {
            view: view.Value,
            len,
        })
    }
}

#[cfg(windows)]
impl Deref for Map {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `view` is a live view of the whole file, `len` readable
        // bytes until `drop`, and the file is never written (the module's
        // doc).
        unsafe { std::slice::from_raw_parts(self.view.cast::<u8>(), self.len) }
    }
}

#[cfg(windows)]
impl Drop for Map {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        if self.len > 0 {
            // SAFETY: the view made in `open`, unmapped once: no slice of it
            // outlives `self`, which `deref` borrows.
            let _ = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view }) };
        }
    }
}

/// The file's bytes, read into memory where no mapping is made.
#[cfg(not(any(unix, windows)))]
pub struct Map(Box<[u8]>);

#[cfg(not(any(unix, windows)))]
impl Map {
    pub fn open(path: &Path) -> std::io::Result<Map> {
        Ok(Map(std::fs::read(path)?.into_boxed_slice()))
    }
}

#[cfg(not(any(unix, windows)))]
impl Deref for Map {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skwird_testkit::Scratch;

    #[test]
    fn a_mapped_file_reads_as_its_bytes() {
        let s = Scratch::new("map");
        let path = s.write(
            "bytes",
            &(0..=255u8).cycle().take(10_000).collect::<Vec<_>>(),
        );
        let map = Map::open(&path).unwrap();
        assert_eq!(map.len(), 10_000);
        assert_eq!(map[300], 44);
        let empty = Map::open(&s.file("empty")).unwrap();
        assert!(empty.is_empty());
    }

    /// A file whose view is alive may be removed or kept (Unix removes it;
    /// Windows may refuse while its view is alive), and reads as it was
    /// until the view is let go of; once it is, the file is removed.
    #[test]
    fn a_mapped_file_is_removed_once_let_go_of() {
        let s = Scratch::new("map-removed");
        let path = s.write("bytes", &[7u8; 5000]);
        let map = Map::open(&path).unwrap();
        let while_mapped = std::fs::remove_file(&path);
        eprintln!("removed while mapped: {while_mapped:?}");
        #[cfg(unix)]
        assert!(while_mapped.is_ok());
        assert!(map.iter().all(|&b| b == 7));
        drop(map);
        if while_mapped.is_err() {
            std::fs::remove_file(&path).unwrap();
        }
        assert!(!path.exists());
    }
}
