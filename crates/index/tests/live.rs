//! Live subtrees (`docs/PLAN.md`, phase 2, step 3): a keeper watching
//! the subtrees it is given, applying what is heard, and never claiming
//! live where a watch is missing or events were dropped.

#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use skwird_crawl::{Cancel, Exclusions, Heard, Options, Root, Watch, Watcher};
use skwird_index::keeper::Keeper;
use skwird_index::tree::Limits;
use skwird_index::{Index, Query};
use skwird_testkit::Scratch;

fn paths(index: &Index) -> BTreeSet<PathBuf> {
    index
        .query(index.root().unwrap(), &Query::default())
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .filter(|p| !p.starts_with("index"))
        .collect()
}

/// A keeper of the scratch tree, with a watcher of `budget` watches
/// wanting `scopes` (below the root), opened and given them.
fn watching(s: &Scratch, scopes: &[&str], budget: usize) -> (Keeper, Arc<Watcher>) {
    let root = Root::open(s.path(), Exclusions::none()).unwrap();
    let watcher = Arc::new(Watcher::new(1, budget).unwrap());
    watcher.set_scopes(scopes.iter().map(|p| root.path().join(p)).collect());
    let opts = Options {
        watcher: Some(watcher.clone()),
        threads: 3,
        ..Options::default()
    };
    let limits = Limits {
        target: 40,
        max: 60,
        min: 15,
    };
    let (mut k, _) =
        Keeper::open(&s.join("index"), root, opts, limits, &Cancel::default()).unwrap();
    let rels: Vec<PathBuf> = scopes.iter().map(PathBuf::from).collect();
    k.set_scopes(&rels, &Cancel::default()).unwrap();
    (k, watcher)
}

/// Apply everything heard, as the engine does, until nothing more is: a
/// folder heard by its entries' names stats those alone, one heard of
/// itself is read whole. Gives whether an overflow was heard.
fn apply(k: &mut Keeper, w: &Watcher) -> bool {
    let mut overflowed = false;
    loop {
        let mut heard = Vec::new();
        w.read(&mut heard).unwrap();
        if heard.is_empty() {
            return overflowed;
        }
        let mut folders: BTreeMap<u32, Option<HashSet<Box<[u8]>>>> = BTreeMap::new();
        for h in heard {
            match h {
                Heard::Changed(wd) => {
                    if let Some(f) = k.watched(wd) {
                        folders.insert(f, None);
                    }
                }
                Heard::Entry(wd, name) => {
                    if let Some(f) = k.watched(wd) {
                        let names = folders.entry(f).or_insert_with(|| Some(HashSet::new()));
                        if let Some(names) = names {
                            names.insert(name);
                        }
                    }
                }
                Heard::Overflow => overflowed = true,
                Heard::Moved(_) | Heard::Gone(_) | Heard::Cookie(_) => {}
            }
        }
        if overflowed {
            k.overflowed();
            return true;
        }
        let folders: Vec<_> = folders.into_iter().collect();
        k.heard_named(&folders, &Cancel::default()).unwrap();
    }
}

/// Installed, a subtree is live from when its install began, and the
/// folders outside it are not.
#[test]
fn an_installed_subtree_is_live_and_only_it() {
    let s = Scratch::new("live-installed");
    s.file("docs/a/one.txt");
    s.file("docs/b/two.txt");
    s.file("other/three.txt");
    let before = skwird_crawl::Time::now();
    let (k, w) = watching(&s, &["docs"], 100);
    assert_eq!(w.watching(), 3, "docs, docs/a and docs/b");
    let live = &k.live()[0];
    assert_eq!(live.short, None);
    let since = live.since.expect("live");
    assert!(since >= before);
    let index = k.index();
    assert_eq!(index.live_since(Path::new("docs/a")), Some(since));
    assert_eq!(index.live_since(Path::new("docs")), Some(since));
    assert_eq!(index.live_since(Path::new("other")), None);
    assert_eq!(index.live_since(Path::new("")), None);
}

/// A folder made, filled and moved within a watched subtree is applied
/// as heard: its new folders are watched before they are read, so what is
/// made in them after is heard too.
#[test]
fn a_folder_made_filled_and_moved_within_the_subtree_is_applied() {
    let s = Scratch::new("live-made");
    s.dir("docs/a");
    let (mut k, w) = watching(&s, &["docs"], 100);
    s.file("docs/a/new.txt");
    s.file("docs/c/d/e/deep.txt");
    assert!(!apply(&mut k, &w));
    let got = paths(k.index());
    assert!(got.contains(Path::new("docs/a/new.txt")), "{got:?}");
    assert!(got.contains(Path::new("docs/c/d/e/deep.txt")), "{got:?}");
    // Made in folders crawled since: heard, so watched.
    s.file("docs/c/d/e/later.txt");
    s.rename("docs/c", "docs/moved");
    assert!(!apply(&mut k, &w));
    let got = paths(k.index());
    assert!(
        got.contains(Path::new("docs/moved/d/e/later.txt")),
        "{got:?}"
    );
    assert!(!got.iter().any(|p| p.starts_with("docs/c")), "{got:?}");
    assert!(k.live()[0].since.is_some(), "still live");
}

/// A watched folder holding another, moved from one live subtree to
/// another, its old parent read first in the batch: the folder within is
/// still heard. Inotify gives a folder moved the descriptors it had, and
/// the moved folder was given the number its inner folder had just let
/// go of: that number's old descriptor, the inner folder's own, was let
/// go of while the inner folder was noted as watched by it.
#[test]
fn a_folder_moved_between_subtrees_keeps_the_watches_within() {
    let s = Scratch::new("live-between");
    s.file("p/x/y/one.txt");
    s.dir("q");
    let (mut k, w) = watching(&s, &["p", "q"], 100);
    assert_eq!(w.watching(), 4, "p, p/x, p/x/y and q");
    s.rename("p/x", "q/x");
    assert!(!apply(&mut k, &w));
    let got = paths(k.index());
    assert!(got.contains(Path::new("q/x/y/one.txt")), "{got:?}");
    assert!(!got.iter().any(|p| p.starts_with("p/x")), "{got:?}");
    s.file("q/x/y/after.txt");
    assert!(!apply(&mut k, &w));
    let got = paths(k.index());
    assert!(got.contains(Path::new("q/x/y/after.txt")), "{got:?}");
    assert!(k.live().iter().all(|l| l.since.is_some()), "still live");
}

/// A folder moved out of the subtree and back: nothing of it stays under
/// its old path while it is out, and it is back, read, once returned.
#[test]
fn a_folder_moved_out_and_back_leaves_nothing_behind() {
    let s = Scratch::new("live-out");
    s.file("docs/a/inner/file.txt");
    s.dir("other");
    let (mut k, w) = watching(&s, &["docs"], 100);
    s.rename("docs/a", "other/a");
    apply(&mut k, &w);
    let got = paths(k.index());
    assert!(!got.iter().any(|p| p.starts_with("docs/a")), "{got:?}");
    s.rename("other/a", "docs/a");
    apply(&mut k, &w);
    assert!(paths(k.index()).contains(Path::new("docs/a/inner/file.txt")));
    s.file("docs/a/inner/after.txt");
    apply(&mut k, &w);
    assert!(paths(k.index()).contains(Path::new("docs/a/inner/after.txt")));
}

/// An overflow ends "live" at once; the refresh after it reads what the
/// dropped events held, and brings "live" back from when it began.
#[test]
fn an_overflow_ends_live_and_the_check_after_brings_it_back() {
    let s = Scratch::new("live-overflow");
    s.dir("docs/busy");
    let (mut k, w) = watching(&s, &["docs"], 100);
    let most: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for i in 0..most + 64 {
        std::fs::write(s.join(format!("docs/busy/{i}")), b"").unwrap();
    }
    s.file("docs/made/late.txt");
    assert!(apply(&mut k, &w), "the overflow heard");
    assert_eq!(k.live()[0].since, None);
    assert_eq!(k.index().live_since(Path::new("docs")), None);
    let before = skwird_crawl::Time::now();
    k.refresh(&Cancel::default()).unwrap();
    let since = k.live()[0].since.expect("live again");
    assert!(since >= before);
    let got = paths(k.index());
    assert!(got.contains(Path::new("docs/made/late.txt")));
    assert!(got.contains(&PathBuf::from(format!("docs/busy/{}", most + 63))));
    // The folder made while events were dropped is watched now.
    s.file("docs/made/after.txt");
    assert!(!apply(&mut k, &w));
    assert!(paths(k.index()).contains(Path::new("docs/made/after.txt")));
}

/// A subtree with a folder past the budget of watches is short of live,
/// says why, is never claimed live, and takes none of the budget: what it
/// got is let go of, for the subtrees kept after it.
#[test]
fn a_budget_reached_keeps_a_subtree_from_live() {
    let s = Scratch::new("live-budget");
    for f in ["docs/a", "docs/b", "docs/c"] {
        s.dir(f);
    }
    let (k, w) = watching(&s, &["docs"], 2);
    assert_eq!(w.watching(), 0);
    let live = &k.live()[0];
    assert_eq!(live.short, Some(Watch::Budget));
    assert_eq!(live.since, None);
    assert_eq!(k.index().live_since(Path::new("docs")), None);
}

/// A folder removed lets go of its watch.
#[test]
fn a_folder_removed_lets_go_of_its_watch() {
    let s = Scratch::new("live-removed");
    s.file("docs/a/b/file.txt");
    let (mut k, w) = watching(&s, &["docs"], 100);
    assert_eq!(w.watching(), 3);
    s.remove("docs/a");
    apply(&mut k, &w);
    assert_eq!(w.watching(), 1, "only docs is watched");
    assert!(!paths(k.index()).iter().any(|p| p.starts_with("docs/a")));
}

/// Register `scopes` (below the root) with the keeper and its watcher.
fn register(k: &mut Keeper, w: &Watcher, s: &Scratch, scopes: &[&str]) {
    let base = skwird_crawl::canonical(s.path()).unwrap();
    w.set_scopes(scopes.iter().map(|p| base.join(p)).collect());
    let rels: Vec<PathBuf> = scopes.iter().map(PathBuf::from).collect();
    k.set_scopes(&rels, &Cancel::default()).unwrap();
}

/// A folder made after the index last read its parent, then registered:
/// read from its parent first, crawled and watched, then live. It was
/// once installed with nothing in it to watch, and claimed live.
#[test]
fn a_folder_made_after_the_index_was_read_is_watched_when_registered() {
    let s = Scratch::new("live-made-later");
    s.dir("docs");
    let (mut k, w) = watching(&s, &[], 100);
    s.file("later/inner/first.txt");
    register(&mut k, &w, &s, &["later"]);
    assert_eq!(w.watching(), 2, "later and later/inner");
    assert!(k.live()[0].since.is_some());
    assert!(paths(k.index()).contains(Path::new("later/inner/first.txt")));
    s.file("later/inner/second.txt");
    apply(&mut k, &w);
    assert!(paths(k.index()).contains(Path::new("later/inner/second.txt")));
}

/// A folder registered before it exists is not live; once made, the
/// refresh that finds it installs it, and it is.
#[test]
fn a_folder_registered_before_it_exists_is_live_once_made() {
    let s = Scratch::new("live-not-yet");
    s.dir("docs");
    let (mut k, w) = watching(&s, &[], 100);
    register(&mut k, &w, &s, &["ghost"]);
    assert_eq!(k.live()[0].since, None);
    assert_eq!(k.index().live_since(Path::new("ghost")), None);
    s.file("ghost/made.txt");
    k.refresh(&Cancel::default()).unwrap();
    assert!(k.live()[0].since.is_some(), "{:?}", k.live());
    s.file("ghost/after.txt");
    apply(&mut k, &w);
    assert!(paths(k.index()).contains(Path::new("ghost/after.txt")));
}

/// A large folder heard by one of its entries stats that entry alone,
/// the others kept as they were read; the entry written to is read again,
/// its new size in the index. Each read of such a folder once stat-ed
/// every entry: a home's busy cache folder of 62,000 entries cost 7.5% of
/// a core.
#[test]
fn a_folder_heard_by_an_entry_stats_that_entry_alone() {
    let s = Scratch::new("live-named");
    for i in 0..2000 {
        s.file(format!("docs/big/f{i:04}"));
    }
    let (mut k, w) = watching(&s, &["docs"], 100);
    std::fs::write(s.join("docs/big/f0042"), b"grown").unwrap();
    let mut heard = Vec::new();
    w.read(&mut heard).unwrap();
    let mut folder = None;
    let mut names = HashSet::new();
    for h in heard {
        if let Heard::Entry(wd, name) = h {
            folder = k.watched(wd);
            names.insert(name);
        }
    }
    let done = k
        .heard_named(&[(folder.expect("heard"), Some(names))], &Cancel::default())
        .unwrap();
    assert!(done.entries <= 2, "{} entries stat-ed", done.entries);
    let index = k.index();
    let size = index
        .query(index.root().unwrap(), &Query::default())
        .unwrap()
        .into_iter()
        .find(|h| h.path == Path::new("docs/big/f0042"))
        .and_then(|h| h.attrs)
        .map(|a| a.size);
    assert_eq!(size, Some(5), "the entry written to read again");
    assert_eq!(
        paths(index)
            .iter()
            .filter(|p| p.starts_with("docs/big"))
            .count(),
        2001
    );
}
