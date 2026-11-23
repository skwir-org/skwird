//! Tests of the crawl and the check on Linux alone (`docs/PLAN.md`,
//! phase 1, the fixtures): btrfs, folders denied by their modes, the
//! listing read in place, paths past what the system resolves at once,
//! the mount table, mounts in a namespace of the test's own. What every
//! system does is tested in `walk`'s tests, from which these take their
//! helpers.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use skwird_testkit::{Scratch, in_namespace};

use super::*;
use crate::walk::tests::{listed, not_covered, open, opts, stored, verdicts, walked};
use crate::{Cancel, Stored, StoredFolder, crawl_all, read_folder};

/// A btrfs subvolume below the root has a device of its own, and is no
/// mount: it is crawled as part of the tree. Skipped where none can be
/// made.
#[test]
fn a_btrfs_subvolume_is_crawled_with_its_own_device() {
    let s = Scratch::new("crawl-subvolume");
    s.dir("plain");
    if !s.subvolume("sub") {
        eprintln!("skipped: no btrfs subvolume can be made here");
        return;
    }
    s.file("sub/file");
    let root = open(&s);
    let crawl = crawl_all(&root, &opts(2), &Cancel::default()).unwrap();
    assert!(not_covered(&crawl).is_empty());
    let list = listed(&crawl);
    assert!(list.contains_key(Path::new("sub/file")));
    assert_ne!(
        list[Path::new("sub")].1.dev,
        list[Path::new("plain")].1.dev,
        "the subvolume's own device"
    );
    assert_eq!(list, walked(s.path()));
}

/// A folder that may not be read is not covered, never empty; made
/// readable, the next check reads it again though its times did not
/// tell.
#[test]
fn a_denied_folder_is_not_covered_then_read_at_the_next_check() {
    let s = Scratch::new("crawl-denied");
    s.file("open/a");
    s.file("shut/b");
    s.deny("shut");
    if std::fs::read_dir(s.join("shut")).is_ok() {
        eprintln!("skipped: permissions do not hold here (root?)");
        return;
    }
    let root = open(&s);
    let crawl = crawl_all(&root, &opts(2), &Cancel::default()).unwrap();
    assert_eq!(
        not_covered(&crawl).get(Path::new("shut")),
        Some(&Why::Denied)
    );
    assert!(!listed(&crawl).contains_key(Path::new("shut/b")));
    let stored = stored(&crawl);
    s.allow("shut");
    assert_eq!(
        verdicts(&root, &stored).get(Path::new("shut")),
        Some(&Stale::Again)
    );
    let (_, outcome) = read_folder(&root, Path::new("shut"), &opts(1));
    let Outcome::Listed(listing) = outcome else {
        panic!("read once allowed: {outcome:?}")
    };
    assert_eq!(listing.name(0), b"b");
}

/// A listing's names, read in place: `.` and `..` left out, each with its
/// kind, sorted by their bytes, a name before the longer ones it starts.
#[test]
fn a_listing_s_names_are_read_in_place_and_sorted() {
    let s = Scratch::new("crawl-names-sorted");
    for name in ["b", "ab", "a b", "a"] {
        s.file(name);
    }
    s.dir("c");
    let fd = rustix::fs::open(s.path(), dir_flags(), Mode::empty()).unwrap();
    let mut names = names_of(&fd).unwrap();
    names.sort();
    let seen: Vec<(Vec<u8>, FileType)> = names
        .iter()
        .map(|(name, _, kind, _)| (name.to_vec(), kind))
        .collect();
    let want: Vec<(Vec<u8>, FileType)> = [
        ("a", FileType::RegularFile),
        ("a b", FileType::RegularFile),
        ("ab", FileType::RegularFile),
        ("b", FileType::RegularFile),
        ("c", FileType::Directory),
    ]
    .into_iter()
    .map(|(n, k)| (n.as_bytes().to_vec(), k))
    .collect();
    assert_eq!(seen, want);
}

/// Folders deeper than the system resolves at once are checked through
/// their parent's handle: current, then changed.
#[test]
fn the_check_reaches_folders_deeper_than_a_path_can_name() {
    let s = Scratch::new("check-deep");
    let bottom = s.deep("deep", 300, "level-of-twenty-bytes");
    let root = open(&s);
    let crawl = crawl_all(
        &root,
        &Options {
            tick: Duration::ZERO,
            ..opts(2)
        },
        &Cancel::default(),
    )
    .unwrap();
    let stored = stored(&crawl);
    assert_eq!(stored.len(), 302, "the root, deep, and 300 levels");
    assert!(verdicts(&root, &stored).is_empty());
    // Add a file at the bottom through its handle, as no path reaches it.
    let fd = open_beneath(&root.dir().unwrap(), &bottom).unwrap();
    rustix::fs::openat(
        &fd,
        "added",
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .unwrap();
    let found = verdicts(&root, &stored);
    assert_eq!(found, BTreeMap::from([(bottom, Stale::Changed)]));
}

/// A mount table line for a mount at `point`, its spaces escaped as the
/// kernel escapes them.
fn table_line(id: u64, point: &Path, fstype: &str, source: &str) -> String {
    let point = point.to_str().unwrap().replace(' ', "\\040");
    format!("{id} 1 0:{id} / {point} rw - {fstype} {source} rw\n")
}

/// A network mount below the root is never entered, nor stat-ed: told
/// from the mount table alone, as skwir tells it. A root on one is
/// refused before it is opened.
#[test]
fn a_network_mount_below_the_root_is_never_entered() {
    let s = Scratch::new("crawl-network");
    // Plain folders, which the table given says are mounts.
    s.file("nas/photo.jpg");
    s.file("stick/song.ogg");
    s.file("home/notes.txt");
    let table = Table::parse(
        format!(
            "{}{}",
            table_line(9001, &s.join("nas"), "nfs4", "nas:/export"),
            table_line(9002, &s.join("stick"), "vfat", "/dev/sdx1"),
        )
        .as_bytes(),
    );
    let root = Root::open_with(s.path(), Exclusions::none(), table.clone()).unwrap();
    let crawl = crawl_all(&root, &opts(2), &Cancel::default()).unwrap();
    let not = not_covered(&crawl);
    assert_eq!(not.get(Path::new("nas")), Some(&Why::NetworkMount));
    assert_eq!(not.get(Path::new("stick")), Some(&Why::Mount));
    let list = listed(&crawl);
    assert!(
        list.contains_key(Path::new("nas")),
        "its name, in its parent"
    );
    assert!(list[Path::new("nas")].2.is_none(), "never stat-ed");
    assert!(!list.contains_key(Path::new("nas/photo.jpg")));
    assert!(!list.contains_key(Path::new("stick/song.ogg")));
    assert!(list.contains_key(Path::new("home/notes.txt")));
    let (_, outcome) = read_folder(&root, Path::new("nas"), &opts(1));
    assert!(matches!(outcome, Outcome::NotCovered(Why::NetworkMount)));
    // The check stats nothing at or below a network mount.
    let mut stored = Stored::default();
    stored.push(Path::new("nas"), StoredFolder::default());
    assert_eq!(
        verdicts(&root, &stored).get(Path::new("nas")),
        Some(&Stale::NetworkMount)
    );
    // A root on a network mount is refused, by its spelling.
    let on_share = Table::parse(table_line(9003, s.path(), "cifs", "//nas/share").as_bytes());
    assert!(matches!(
        Root::open_with(&s.join("home"), Exclusions::none(), on_share),
        Err(Error::Network)
    ));
}

/// Mounts below the root, a tmpfs and a bind mount of a folder of the
/// same filesystem, are not crossed: not by the crawl, not by a read that
/// starts at the mount or below it. A mount made after the crawl is
/// found by the check, from the table or by its mount id.
#[test]
fn mounts_below_the_root_are_not_crossed() {
    if !in_namespace() {
        return;
    }
    let s = Scratch::new("crawl-mounts");
    s.file("a/x/file");
    s.mount_tmpfs("m");
    s.file("m/inside");
    s.mount_bind("a", "b");
    let root = open(&s);
    // A tick of zero, so that the check stats every folder.
    let no_tick = Options {
        tick: Duration::ZERO,
        ..opts(2)
    };
    let crawl = crawl_all(&root, &no_tick, &Cancel::default()).unwrap();
    let not = not_covered(&crawl);
    assert_eq!(not.get(Path::new("m")), Some(&Why::Mount));
    assert_eq!(not.get(Path::new("b")), Some(&Why::Mount), "a bind mount");
    let list = listed(&crawl);
    assert!(!list.contains_key(Path::new("m/inside")));
    assert!(!list.contains_key(Path::new("b/x")));
    assert!(list.contains_key(Path::new("a/x/file")));
    for rel in ["m", "b", "b/x"] {
        let (_, outcome) = read_folder(&root, Path::new(rel), &opts(1));
        assert!(
            matches!(outcome, Outcome::NotCovered(Why::Mount)),
            "{rel}: {outcome:?}"
        );
    }
    let stored = stored(&crawl);
    s.mount_tmpfs("a/x");
    // Without the table read again, by its mount id.
    assert_eq!(
        verdicts(&root, &stored).get(Path::new("a/x")),
        Some(&Stale::Mount)
    );
    let mut root = root;
    root.reread_mounts().unwrap();
    assert_eq!(
        verdicts(&root, &stored).get(Path::new("a/x")),
        Some(&Stale::Mount)
    );
}

/// A folder lists what was made after it was opened only once opened
/// again (or rewound): btrfs leaves it out since its fix for endless
/// reads, as POSIX allows. A watched folder is listed through the folder
/// opened again after its watch (`read`), so that what is made between
/// its open and its watch is listed; before, it was neither listed nor
/// heard, and the user's engine missed it on btrfs.
#[test]
fn a_folder_opened_again_lists_what_was_made_after_its_open() {
    let s = Scratch::new("opened-again");
    s.file("d/first");
    let fd = rustix::fs::open(s.join("d"), dir_flags(), Mode::empty()).unwrap();
    s.file("d/made after the open");
    let names = |fd: &OwnedFd| -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = names_of(fd)
            .unwrap()
            .iter()
            .map(|(n, ..)| n.to_vec())
            .collect();
        names.sort();
        names
    };
    let again = opened_again(&fd).unwrap();
    assert_eq!(
        names(&again),
        [b"first".to_vec(), b"made after the open".to_vec()]
    );
    // What the folder opened first lists depends on its filesystem.
    let first = names(&fd);
    assert!(first.contains(&b"first".to_vec()), "{first:?}");
}
