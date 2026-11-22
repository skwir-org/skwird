//! What the crawl asks btrfs itself (`docs/DESIGN.md`, open question 10,
//! answered on 2026-11-22): whether a subvolume is read-only, as a
//! snapshot is. A snapshot below a root is left out, its name kept: each
//! would be another copy of what it was taken of.
#![allow(unsafe_code)] // an ioctl rustix has no binding for; the block says why it is sound.

use rustix::fd::{AsRawFd, BorrowedFd};

/// `BTRFS_IOC_SUBVOL_GETFLAGS`: `_IOR(0x94, 25, __u64)`.
const SUBVOL_GETFLAGS: u64 = 0x8008_9419;
/// `BTRFS_SUBVOL_RDONLY`.
const SUBVOL_RDONLY: u64 = 1 << 1;
/// The inode of every btrfs subvolume's top folder.
pub const SUBVOLUME_TOP: u64 = 256;

/// Whether the open folder `fd`, a btrfs subvolume's top, is read-only.
/// No if it cannot be asked: it is then read as any folder.
pub fn read_only_subvolume(fd: BorrowedFd<'_>) -> bool {
    let mut flags: u64 = 0;
    // SAFETY: the request writes one `u64` through the pointer, which is
    // valid for that write; `fd` is an open folder.
    let done = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            SUBVOL_GETFLAGS as libc::Ioctl,
            &raw mut flags,
        )
    };
    done == 0 && flags & SUBVOL_RDONLY != 0
}
