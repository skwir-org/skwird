//! Tests of the crawl and the check on Linux (`docs/PLAN.md`, phase 1,
//! the fixtures): each is a way coverage could be overstated, or names
//! read under the wrong path.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use skwird_testkit::{Scratch, in_namespace, reference_walk};

use super::*;

/// Every entry a crawl listed, by its path below the root: its kind, its
/// identity and, for a file, its size.
fn listed(crawl: &Crawl) -> BTreeMap<PathBuf, (Kind, FileId, Option<u64>)> {
    let mut out = BTreeMap::new();
    for folder in &crawl.folders {
        if let Outcome::Listed(listing) = &folder.outcome {
            for (name, entry) in listing.iter() {
                let size = entry
                    .attrs
                    .filter(|_| entry.kind == Kind::File)
                    .map(|a| a.size);
                out.insert(
                    folder.rel.join(component(name)),
                    (entry.kind, entry.id, size),
                );
            }
        }
    }
    out
}

/// Every folder a crawl reached but did not read, with why.
fn not_covered(crawl: &Crawl) -> BTreeMap<PathBuf, Why> {
    crawl
        .folders
        .iter()
        .filter_map(|f| match f.outcome {
            Outcome::NotCovered(why) => Some((f.rel.clone(), why)),
            Outcome::Listed(_) => None,
        })
        .collect()
}

/// What the reference walk saw, in the crawl's terms.
fn walked(root: &Path) -> BTreeMap<PathBuf, (Kind, FileId, Option<u64>)> {
    reference_walk(root)
        .entries
        .into_iter()
        .map(|(path, seen)| {
            let kind = match seen.kind {
                skwird_testkit::Kind::File => Kind::File,
                skwird_testkit::Kind::Folder => Kind::Folder,
                skwird_testkit::Kind::Link => Kind::Link,
                skwird_testkit::Kind::Other => Kind::Other,
            };
            let id = FileId {
                dev: seen.dev,
                ino: seen.ino,
            };
            (path, (kind, id, (kind == Kind::File).then_some(seen.size)))
        })
        .collect()
}

fn opts(threads: usize) -> Options {
    Options {
        threads,
        ..Options::default()
    }
}

fn open(s: &Scratch) -> Root {
    Root::open(s.path(), Exclusions::none()).unwrap()
}

fn stored(crawl: &Crawl) -> Stored {
    let mut stored = Stored::default();
    for f in &crawl.folders {
        let Some(stat) = f.stat else { continue };
        let again = match &f.outcome {
            Outcome::Listed(listing) => listing.again,
            Outcome::NotCovered(why) => why.retried(),
        };
        stored.push(
            &f.rel,
            StoredFolder {
                id: stat.id,
                ctime: stat.ctime,
                again,
            },
        );
    }
    stored
}

/// The check's verdict for each folder, by its path.
fn verdicts(root: &Root, stored: &Stored) -> BTreeMap<PathBuf, Stale> {
    check(root, stored, &opts(3), &Cancel::default())
        .unwrap()
        .stale
        .into_iter()
        .map(|(i, why)| (stored.rel(i).to_path_buf(), why))
        .collect()
}

/// The paths of folders given one after the other read back each its own,
/// the root's empty one, names not UTF-8 and a path past the system's
/// limit among them.
#[test]
fn stored_paths_read_back_each_its_own() {
    let deep = "d/".repeat(3000);
    let paths = [
        PathBuf::new(),
        PathBuf::from("a"),
        PathBuf::from(OsStr::from_bytes(b"latin1-\xe9t\xe9/x")),
        PathBuf::from(deep.trim_end_matches('/')),
        PathBuf::from("a b/c"),
    ];
    let mut stored = Stored::with_capacity(paths.len(), 64);
    for (n, p) in paths.iter().enumerate() {
        let folder = StoredFolder {
            id: FileId {
                dev: 1,
                ino: n as u64,
            },
            ..StoredFolder::default()
        };
        stored.push(p, folder);
    }
    assert_eq!(stored.len(), paths.len());
    for (i, p) in paths.iter().enumerate() {
        assert_eq!(stored.rel(i), p.as_path());
        assert_eq!(stored.folder(i).id.ino, i as u64);
    }
}

/// A tree that holds what trips walkers up: spaces, Unicode, bytes that
/// are not UTF-8, a link to a folder and a dangling one, hard links, a
/// pipe, and folders deeper than 256 levels whose paths are longer than
/// the system resolves at once.
fn awkward(s: &Scratch) -> PathBuf {
    s.file("a name with spaces.txt");
    s.write("été/naïve 東京.md", b"unicode");
    s.file(Path::new(OsStr::from_bytes(b"latin1-\xe9t\xe9")));
    s.dir(Path::new(OsStr::from_bytes(b"dir-\xff\xfe")));
    s.file(Path::new(OsStr::from_bytes(b"dir-\xff\xfe/inner")));
    s.write("data/file.bin", &[7; 1000]);
    s.hard_link("data/same.bin", "data/file.bin");
    s.link("to-data", "data");
    s.link("dangling", "nowhere");
    let made = std::process::Command::new("mkfifo")
        .arg(s.join("data/pipe"))
        .status();
    assert!(made.is_ok_and(|st| st.success()), "cannot make a pipe");
    s.deep("deep", 300, "level-of-twenty-bytes")
}

/// A crawl lists what a plain walk finds, entry for entry, whatever the
/// names, the depth and the number of threads.
#[test]
fn a_crawl_lists_what_a_walk_finds() {
    let s = Scratch::new("crawl-equals-walk");
    let bottom = awkward(&s);
    assert!(s.join(&bottom).as_os_str().len() > PATH_MAX);
    let root = open(&s);
    let walk = walked(s.path());
    for threads in [1, 4] {
        let crawl = crawl_all(&root, &opts(threads), &Cancel::default()).unwrap();
        assert_eq!(listed(&crawl), walk, "on {threads} threads");
        assert!(not_covered(&crawl).is_empty(), "{:?}", not_covered(&crawl));
        assert_eq!(crawl.folders[0].rel, Path::new(""));
        assert!(crawl.folders[1..].iter().all(|f| f.parent.is_some()));
    }
    let crawl = crawl_all(&root, &opts(2), &Cancel::default()).unwrap();
    let list = listed(&crawl);
    assert!(
        list.contains_key(&bottom.join("bottom")),
        "the deepest file"
    );
    assert_eq!(
        list[Path::new("data/file.bin")].1,
        list[Path::new("data/same.bin")].1,
        "two names of one file, one identity"
    );
    assert_eq!(
        list[Path::new("to-data")].0,
        Kind::Link,
        "a link is not followed"
    );
    assert_eq!(list[Path::new("data/pipe")].0, Kind::Other);
}

/// Without attributes asked, names and kinds are the same, and only
/// folders are stat-ed.
#[test]
fn a_crawl_of_names_alone_lists_the_same_names() {
    let s = Scratch::new("crawl-names-alone");
    awkward(&s);
    let root = open(&s);
    let names = Options {
        attrs: false,
        ..opts(3)
    };
    let crawl = crawl_all(&root, &names, &Cancel::default()).unwrap();
    let without: BTreeMap<_, _> = listed(&crawl).into_iter().map(|(p, e)| (p, e.0)).collect();
    let walk: BTreeMap<_, _> = walked(s.path())
        .into_iter()
        .map(|(p, e)| (p, e.0))
        .collect();
    assert_eq!(without, walk);
}

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

/// A folder holding `.private`, and a folder an exclusion names however
/// it is written, are never read: their own names are listed in their
/// parent, nothing below them is.
#[test]
fn private_and_excluded_folders_are_never_read() {
    let s = Scratch::new("crawl-private");
    s.file("diary/.private");
    s.file("diary/entry.txt");
    s.file("keys/key.pem");
    s.file("keys2/other.txt");
    s.link("to-keys", "keys");
    for written in ["keys/", "keys", "to-keys"] {
        let (exclusions, refused) =
            Exclusions::resolve([format!("{}//{written}", s.path().display())]);
        assert!(refused.is_empty(), "{refused:?}");
        let root = Root::open(s.path(), exclusions).unwrap();
        let crawl = crawl_all(&root, &opts(2), &Cancel::default()).unwrap();
        let list = listed(&crawl);
        let not = not_covered(&crawl);
        assert_eq!(not.get(Path::new("diary")), Some(&Why::Private));
        assert_eq!(
            not.get(Path::new("keys")),
            Some(&Why::Excluded),
            "{written}"
        );
        assert_eq!(not.len(), 2);
        assert!(list.contains_key(Path::new("diary")));
        assert!(list.contains_key(Path::new("keys")));
        assert!(
            !list
                .keys()
                .any(|p| p.starts_with("diary") && p != Path::new("diary"))
        );
        assert!(
            !list
                .keys()
                .any(|p| p.starts_with("keys") && p != Path::new("keys"))
        );
        assert!(list.contains_key(Path::new("keys2/other.txt")));
        // Read again on its own, it is not read either.
        let (_, outcome) = read_folder(&root, Path::new("keys"), &opts(1));
        assert!(matches!(outcome, Outcome::NotCovered(Why::Excluded)));
    }
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

/// A listing that fails part-way is not taken for complete: what was
/// read of it is not kept.
#[test]
fn a_listing_cut_short_gives_its_error_alone() {
    let named = |name: &str| Ok((CString::new(name).unwrap(), FileType::RegularFile, 1));
    let whole = names_of([named("."), named("a"), named("b")].into_iter()).unwrap();
    assert_eq!(whole.len(), 2);
    let cut = names_of([named("a"), Err(Errno::IO), named("b")].into_iter());
    assert_eq!(cut, Err(Errno::IO.raw_os_error()));
}

/// The check finds what changed, and nothing else: a name added, a
/// folder whose times `tar` set back, a folder renamed away, a folder
/// removed, a folder replaced by a file.
#[test]
fn the_check_finds_the_folders_whose_entries_changed() {
    let s = Scratch::new("check-changes");
    for dir in [
        "added",
        "set-back",
        "moved/away",
        "removed",
        "to-file",
        "quiet",
    ] {
        s.dir(dir);
    }
    let root = open(&s);
    // A tick of zero: these folders were all just made, and a test cannot
    // wait for them to age.
    let no_tick = Options {
        tick: Duration::ZERO,
        ..opts(2)
    };
    let crawl = crawl_all(&root, &no_tick, &Cancel::default()).unwrap();
    let stored = stored(&crawl);
    assert!(verdicts(&root, &stored).is_empty(), "nothing changed yet");
    s.file("added/new.txt");
    let mtime = std::fs::metadata(s.join("set-back"))
        .unwrap()
        .modified()
        .unwrap();
    s.file("set-back/unpacked");
    std::fs::File::open(s.join("set-back"))
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    s.rename("moved/away", "moved/elsewhere");
    s.remove("removed");
    s.remove("to-file");
    s.file("to-file");
    let found = verdicts(&root, &stored);
    let expect = BTreeMap::from([
        (PathBuf::from(""), Stale::Changed),
        (PathBuf::from("added"), Stale::Changed),
        (PathBuf::from("set-back"), Stale::Changed),
        (PathBuf::from("moved"), Stale::Changed),
        (PathBuf::from("moved/away"), Stale::Gone),
        (PathBuf::from("removed"), Stale::Gone),
        (PathBuf::from("to-file"), Stale::Replaced),
    ]);
    assert_eq!(found, expect);
}

/// A folder read within a tick of its last change is read again at the
/// next check, though its change time is the one stored: a second change
/// within the tick could have left it so.
#[test]
fn a_folder_read_within_a_tick_of_its_change_is_read_again() {
    let s = Scratch::new("check-tick");
    s.file("fresh/file");
    let root = open(&s);
    let crawl = crawl_all(&root, &opts(1), &Cancel::default()).unwrap();
    let fresh = crawl
        .folders
        .iter()
        .find(|f| f.rel == Path::new("fresh"))
        .unwrap();
    let Outcome::Listed(listing) = &fresh.outcome else {
        panic!("not read")
    };
    assert!(listing.again, "read within two seconds of its making");
    assert_eq!(
        verdicts(&root, &stored(&crawl)).get(Path::new("fresh")),
        Some(&Stale::Again)
    );
}

/// A folder, or a folder above it, replaced by a link: the check says so,
/// and a read of its path never lists the other folder's names.
#[test]
fn a_folder_replaced_by_a_link_never_lists_another_folders_names() {
    let s = Scratch::new("check-link");
    s.file("a/b/mine.txt");
    s.file("z/b/theirs.txt");
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
    s.remove("a");
    s.link("a", "z");
    let found = verdicts(&root, &stored);
    assert_eq!(
        found.get(Path::new("a")),
        Some(&Stale::Replaced),
        "a link now"
    );
    assert_eq!(
        found.get(Path::new("a/b")),
        Some(&Stale::Replaced),
        "z/b, by the link"
    );
    let (_, outcome) = read_folder(&root, Path::new("a/b"), &opts(1));
    assert!(
        matches!(outcome, Outcome::NotCovered(Why::Replaced)),
        "{outcome:?}"
    );
    let (_, outcome) = read_folder(&root, Path::new("a"), &opts(1));
    assert!(matches!(outcome, Outcome::NotCovered(Why::Replaced)));
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
    let fd = open_beneath(&root.fd, &bottom).unwrap();
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

/// A crawl asked to stop says it stopped, never that it finished.
#[test]
fn a_cancelled_crawl_says_so() {
    let s = Scratch::new("crawl-cancel");
    s.file("a/b");
    let root = open(&s);
    let cancel = Cancel::default();
    cancel.cancel();
    assert!(matches!(
        crawl_all(&root, &opts(2), &cancel),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        check(&root, &Stored::default(), &opts(2), &cancel),
        Err(Error::Cancelled)
    ));
}
