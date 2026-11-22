// Root, fanotify and mounts are Linux's here; the other systems' come
// with their phases.
#![cfg(target_os = "linux")]

//! What the crawl does as root, in the test machine (`tools/vm/root.nix`):
//! each test is skipped anywhere else.

use std::os::unix::fs::MetadataExt;

use skwird_testkit::machine;

/// `statfs`'s word for ext4 and for btrfs.
const EXT4: u64 = 0xEF53;
const BTRFS: u64 = 0x9123_683E;

fn kind(path: &std::path::Path) -> u64 {
    rustix::fs::statfs(path).expect("cannot statfs").f_type as u64
}

/// The machine is what the tests below rely on: an ext4 filesystem, and
/// a btrfs one mounted through a subvolume, whose subvolumes each have a
/// device of their own (DESIGN 6).
#[test]
fn the_machine_has_its_filesystems() {
    let Some(m) = machine() else { return };
    assert_eq!(kind(&m.ext4), EXT4);
    assert_eq!(kind(&m.btrfs), BTRFS);
    let top = std::fs::metadata(&m.btrfs).unwrap().dev();
    let a = std::fs::metadata(m.btrfs.join("a")).unwrap().dev();
    let b = std::fs::metadata(m.btrfs.join("b")).unwrap().dev();
    assert!(top != a && a != b && top != b, "{top} {a} {b}");
    // Every subvolume's top folder is inode 256.
    assert_eq!(std::fs::metadata(m.btrfs.join("a")).unwrap().ino(), 256);
}
