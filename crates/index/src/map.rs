//! A file mapped into memory and read in place (`docs/DESIGN.md`,
//! decision 8): its pages are the system's to drop under pressure and
//! read again, so a large index weighs on memory only where it is read.
//!
//! The one place the index speaks to the system with `unsafe`. A mapped
//! file must not change while it is mapped, or what was read as one
//! slice of bytes would change under it. The index never writes a file
//! it has written once: a chunk rewritten is a new file, the old one
//! removed once no snapshot holds it, and each file is its user's alone
//! (mode 0600, in a folder of mode 0700). Elsewhere than Unix, the file
//! is read into memory instead.

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

/// The file's bytes, read into memory where no mapping is made.
#[cfg(not(unix))]
pub struct Map(Box<[u8]>);

#[cfg(not(unix))]
impl Map {
    pub fn open(path: &Path) -> std::io::Result<Map> {
        Ok(Map(std::fs::read(path)?.into_boxed_slice()))
    }
}

#[cfg(not(unix))]
impl Deref for Map {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(all(test, unix))]
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
}
