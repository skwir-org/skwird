//! Folder totals from the index (`docs/DESIGN.md`, Protocol, `totals`;
//! `docs/PLAN.md`, skwir's step E), held to what the tree holds: sizes,
//! allocs and counts of files, folders below, the newest file's time,
//! extensions, a file of two names counted once, `shared` where a name is
//! outside, and a hole where a folder is excluded.

#![cfg(target_os = "linux")]

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use skwird_crawl::{Cancel, Exclusions, Kind, Options, Root, Time, Why};
use skwird_index::keeper::Keeper;
use skwird_index::totals::{Sum, Totals, extension};
use skwird_index::tree::Limits;
use skwird_testkit::Scratch;

fn alloc(s: &Scratch, rel: &str) -> u64 {
    std::fs::metadata(s.join(rel)).unwrap().blocks() * 512
}

fn mtime(s: &Scratch, rel: &str) -> Time {
    let m = std::fs::metadata(s.join(rel)).unwrap();
    Time {
        sec: m.mtime(),
        nsec: u32::try_from(m.mtime_nsec()).unwrap(),
    }
}

fn child<'a>(totals: &'a Totals, name: &str) -> &'a Sum {
    &totals
        .children
        .iter()
        .find(|c| c.name == name.as_bytes())
        .unwrap_or_else(|| panic!("{name}: {:?}", totals.children))
        .sum
}

fn ext(sum: &Sum, e: Option<&str>) -> (u64, u64) {
    sum.by_extension
        .get(&e.map(|e| e.as_bytes().to_vec()))
        .copied()
        .unwrap_or_default()
}

/// The tree the totals are held to, and its index, an excluded folder kept
/// out.
fn indexed(s: &Scratch) -> Keeper {
    s.write("tree/a/one.txt", &[1; 100]);
    s.write("tree/a/b/Two.TXT", &[2; 200]);
    s.write("tree/a/b/photo.JPG", &[3; 300]);
    s.write("tree/big.bin", &vec![4; 70_000]);
    s.write("tree/linked/first", &[5; 500]);
    s.hard_link("tree/elsewhere", "tree/linked/first");
    s.write("tree/twice/x", &[6; 600]);
    s.hard_link("tree/twice/y", "tree/twice/x");
    s.write("tree/excluded/secret.txt", &[7; 700]);
    s.dir("tree/empty");
    let (ex, refused) = Exclusions::resolve([s.join("tree/excluded")]);
    assert!(refused.is_empty(), "{refused:?}");
    let root = Root::open(&s.join("tree"), ex).unwrap();
    let (k, _) = Keeper::open(
        &s.join("index"),
        root,
        Options {
            threads: 2,
            ..Options::default()
        },
        Limits::default(),
        &Cancel::default(),
    )
    .unwrap();
    k
}

/// Each child of the root summed as the tree holds it, and the root's own
/// totals every child's: a file of two names in one folder once and not
/// shared; one with a name in another folder shared there, once at the
/// root, where both are; an excluded folder a hole, nothing in it counted;
/// files dated, folders by their own time beside.
#[test]
fn totals_are_what_the_tree_holds() {
    let s = Scratch::new("totals");
    let k = indexed(&s);
    let index = k.index();
    let top = index.root().unwrap();
    let totals = index.totals(top, None).unwrap();
    let names: Vec<&[u8]> = totals.children.iter().map(|c| c.name.as_slice()).collect();
    assert_eq!(
        names,
        [
            b"a".as_slice(),
            b"big.bin",
            b"elsewhere",
            b"empty",
            b"excluded",
            b"linked",
            b"twice"
        ]
    );
    let a = child(&totals, "a");
    assert_eq!((a.files, a.folders, a.size), (3, 1, 600));
    assert_eq!(
        a.alloc,
        alloc(&s, "tree/a/one.txt")
            + alloc(&s, "tree/a/b/Two.TXT")
            + alloc(&s, "tree/a/b/photo.JPG")
    );
    assert_eq!(ext(a, Some("txt")).0, 2, "Two.TXT lowercased");
    assert_eq!(ext(a, Some("jpg")).0, 1);
    let newest = [
        mtime(&s, "tree/a/one.txt"),
        mtime(&s, "tree/a/b/Two.TXT"),
        mtime(&s, "tree/a/b/photo.JPG"),
    ]
    .into_iter()
    .max();
    assert_eq!(a.newest, newest, "the newest file's time");
    let a_child = totals.children.iter().find(|c| c.name == b"a").unwrap();
    assert_eq!(a_child.kind, Kind::Folder);
    assert_eq!(
        a_child.mtime,
        Some(mtime(&s, "tree/a")),
        "a folder's own time beside"
    );
    let big = child(&totals, "big.bin");
    assert_eq!(
        (big.files, big.size, big.alloc),
        (1, 70_000, alloc(&s, "tree/big.bin"))
    );
    assert_eq!(ext(big, Some("bin")).0, 1);
    let linked = child(&totals, "linked");
    assert_eq!(linked.files, 1);
    assert_eq!(
        linked.shared,
        alloc(&s, "tree/linked/first"),
        "a name outside"
    );
    let twice = child(&totals, "twice");
    assert_eq!(
        (twice.files, twice.size, twice.shared),
        (1, 600, 0),
        "two names inside, once"
    );
    let empty = child(&totals, "empty");
    assert_eq!((empty.files, empty.folders, empty.newest), (0, 0, None));
    let excluded = child(&totals, "excluded");
    assert_eq!(excluded.files, 0, "nothing in an excluded folder counts");
    let hole = index.folder_at(Path::new("excluded")).unwrap();
    assert!(
        totals.holes.contains(&(hole, Why::Excluded)),
        "{:?}",
        totals.holes
    );
    // The root: every child, the file of a name outside `linked` once now
    // that both its names are in, and not shared.
    let own = &totals.own;
    assert_eq!(own.files, 3 + 1 + 1 + 1);
    assert_eq!(own.folders, 6, "a, b, empty, excluded, linked, twice");
    assert_eq!(own.size, 600 + 70_000 + 500 + 600);
    assert_eq!(own.shared, 0);
    assert_eq!(
        ext(own, None),
        (
            2,
            alloc(&s, "tree/linked/first") + alloc(&s, "tree/twice/x")
        )
    );
    assert!(!totals.partial, "statx gives every count of names");
    assert_eq!(totals.mtime, Some(mtime(&s, "tree")));
    assert_eq!(extension(b"photo.JPG"), Some(b"jpg".to_vec()));
}
