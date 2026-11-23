//! Tests of the crawl and the check on every system that has one (Linux,
//! Windows under Wine, `tools/wine-tests`): each is a way coverage could
//! be overstated, or names read under the wrong path. What one system
//! alone has (btrfs, modes, the mount table, namespaces) is tested in its
//! own module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use skwird_testkit::{Scratch, reference_walk};

use crate::sys::component;
use crate::{
    Cancel, Crawl, Error, Exclusions, FileId, Kind, Options, Outcome, Root, Stale, Stored,
    StoredFolder, Why, check, crawl_all, read_folder,
};

/// Every entry a crawl listed, by its path below the root: its kind, its
/// identity and, for a file, its size.
pub(crate) fn listed(crawl: &Crawl) -> BTreeMap<PathBuf, (Kind, FileId, Option<u64>)> {
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
pub(crate) fn not_covered(crawl: &Crawl) -> BTreeMap<PathBuf, Why> {
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
pub(crate) fn walked(root: &Path) -> BTreeMap<PathBuf, (Kind, FileId, Option<u64>)> {
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

pub(crate) fn opts(threads: usize) -> Options {
    Options {
        threads,
        ..Options::default()
    }
}

pub(crate) fn open(s: &Scratch) -> Root {
    Root::open(s.path(), Exclusions::none()).unwrap()
}

pub(crate) fn stored(crawl: &Crawl) -> Stored {
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
pub(crate) fn verdicts(root: &Root, stored: &Stored) -> BTreeMap<PathBuf, Stale> {
    check(root, stored, &opts(3), &Cancel::default())
        .unwrap()
        .stale
        .into_iter()
        .map(|(i, why)| (stored.rel(i).to_path_buf(), why))
        .collect()
}

/// A name no walker may forget, `stem` first: bytes that are not UTF-8 on
/// Unix, an unpaired surrogate on Windows. Wine's names are its host's
/// UTF-8, which holds no unpaired surrogate: under Wine, a character
/// beyond the 16 bits a unit takes, a surrogate pair.
#[cfg(unix)]
fn odd(stem: &str) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(
        &[stem.as_bytes(), b"-\xe9t\xe9"].concat(),
    ))
}

#[cfg(windows)]
fn odd(stem: &str) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let mut wide: Vec<u16> = stem.encode_utf16().collect();
    if skwird_testkit::under_wine() {
        wide.extend("-🙂x".encode_utf16());
    } else {
        wide.extend([u16::from(b'-'), 0xD800, u16::from(b'x')]);
    }
    std::ffi::OsString::from_wide(&wide).into()
}

/// How deep [`awkward`] goes: past the longest path the system resolves
/// at once (4,096 bytes on Linux), or past the 260 characters a plain
/// path takes on Windows.
const DEEP: usize = if cfg!(windows) { 20 } else { 300 };

/// The paths of folders given one after the other read back each its own,
/// the root's empty one, an odd name and a path past the system's limit
/// among them.
#[test]
fn stored_paths_read_back_each_its_own() {
    let deep = "d/".repeat(3000);
    let paths = [
        PathBuf::new(),
        PathBuf::from("a"),
        odd("odd").join("x"),
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

/// A tree that holds what trips walkers up: spaces, Unicode, odd names, a
/// link to a folder and a dangling one, hard links, a pipe where the
/// system has them, and folders deeper than a path can name at once.
fn awkward(s: &Scratch) -> PathBuf {
    s.file("a name with spaces.txt");
    s.write("été/naïve 東京.md", b"unicode");
    s.file(odd("file"));
    s.dir(odd("dir"));
    s.file(odd("dir").join("inner"));
    s.write("data/file.bin", &[7; 1000]);
    s.hard_link("data/same.bin", "data/file.bin");
    s.link("to-data", "data");
    s.link("dangling", "nowhere");
    #[cfg(unix)]
    {
        let made = std::process::Command::new("mkfifo")
            .arg(s.join("data/pipe"))
            .status();
        assert!(made.is_ok_and(|st| st.success()), "cannot make a pipe");
    }
    s.deep("deep", DEEP, "level-of-twenty-bytes")
}

/// A crawl lists what a plain walk finds, entry for entry, whatever the
/// names, the depth and the number of threads.
#[test]
fn a_crawl_lists_what_a_walk_finds() {
    let s = Scratch::new("crawl-equals-walk");
    let bottom = awkward(&s);
    let longest = if cfg!(windows) { 260 } else { 4096 };
    assert!(s.join(&bottom).as_os_str().len() > longest);
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
    assert!(list.contains_key(&odd("dir").join("inner")), "an odd name");
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
    #[cfg(unix)]
    assert_eq!(list[Path::new("data/pipe")].0, Kind::Other);
}

/// Without attributes asked, names and kinds are the same.
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
    s.set_modified("set-back", mtime);
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

/// Folders past what a plain path takes (260 characters) are checked by
/// their verbatim path: current, then changed. Linux's own reaches past
/// what it resolves at once, through a handle.
#[cfg(windows)]
#[test]
fn the_check_reaches_folders_past_a_plain_path() {
    let s = Scratch::new("check-deep");
    let bottom = s.deep("deep", DEEP, "level-of-twenty-bytes");
    assert!(s.join(&bottom).as_os_str().len() > 260);
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
    assert_eq!(stored.len(), DEEP + 2, "the root, deep, and its levels");
    assert!(verdicts(&root, &stored).is_empty());
    s.file(bottom.join("added"));
    let found = verdicts(&root, &stored);
    assert_eq!(found, BTreeMap::from([(bottom, Stale::Changed)]));
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
