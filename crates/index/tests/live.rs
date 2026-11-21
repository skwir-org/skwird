//! Live subtrees (`docs/PLAN.md`, phase 2, step 3): a keeper watching
//! the subtrees it is given, applying what is heard, and never claiming
//! live where a watch is missing or events were dropped.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
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

/// Apply everything heard, as the engine does, until nothing more is.
/// Gives whether an overflow was heard.
fn apply(k: &mut Keeper, w: &Watcher) -> bool {
    let mut overflowed = false;
    loop {
        let mut heard = Vec::new();
        w.read(&mut heard).unwrap();
        if heard.is_empty() {
            return overflowed;
        }
        let mut folders = Vec::new();
        for h in heard {
            match h {
                Heard::Changed(wd) => folders.extend(k.watched(wd)),
                Heard::Overflow => overflowed = true,
                Heard::Moved(_) | Heard::Gone(_) | Heard::Cookie(_) => {}
            }
        }
        if overflowed {
            k.overflowed();
            return true;
        }
        folders.sort_unstable();
        folders.dedup();
        k.heard(&folders, &Cancel::default()).unwrap();
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
/// says why, and is never claimed live.
#[test]
fn a_budget_reached_keeps_a_subtree_from_live() {
    let s = Scratch::new("live-budget");
    for f in ["docs/a", "docs/b", "docs/c"] {
        s.dir(f);
    }
    let (k, w) = watching(&s, &["docs"], 2);
    assert_eq!(w.watching(), 2);
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
    let base = std::fs::canonicalize(s.path()).unwrap();
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
