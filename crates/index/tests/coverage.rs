//! Coverage is never overstated (`docs/PLAN.md`, phase 1, step 3): an
//! index refreshed equals a fresh walk wherever it claims to cover, and
//! every folder the walk could not read is in a subtree it says it does
//! not cover. Fixtures for each way coverage is lost, and random edits,
//! made while the engine is down and between its refreshes.

#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use skwird_crawl::{Cancel, Exclusions, Kind, Options, Root, Time, Why, crawl_all};
use skwird_index::store;
use skwird_index::tree::Limits;
use skwird_index::update::Seen;
use skwird_index::{Index, Proof, Query, build, refresh};
use skwird_testkit::{Scratch, reference_walk};

/// Entries by their path below the root: kind and inode.
type Entries = BTreeMap<PathBuf, (Kind, u64)>;

fn of_index(index: &Index) -> Entries {
    index
        .query(index.root().unwrap(), &Query::default())
        .unwrap()
        .into_iter()
        .map(|h| (h.path, (h.kind, h.id.ino)))
        .collect()
}

fn of_walk(root: &Path) -> (Entries, BTreeSet<PathBuf>) {
    let walk = reference_walk(root);
    let entries = walk
        .entries
        .into_iter()
        .map(|(p, s)| {
            let kind = match s.kind {
                skwird_testkit::Kind::File => Kind::File,
                skwird_testkit::Kind::Folder => Kind::Folder,
                skwird_testkit::Kind::Link => Kind::Link,
                skwird_testkit::Kind::Other => Kind::Other,
            };
            (p, (kind, s.ino))
        })
        .collect();
    (entries, walk.not_read)
}

/// The index covers what it claims: outside its holes it equals the
/// walk, its names are checked, and every folder the walk could not read
/// is a hole or below one.
fn assert_sound(index: &Index, root: &Path, context: &str) {
    let coverage = index.coverage(index.root().unwrap());
    assert!(
        matches!(coverage.names, Proof::Checked(_)),
        "{context}: {:?}",
        coverage.names
    );
    let holes: Vec<&PathBuf> = coverage.holes.iter().map(|h| &h.path).collect();
    let under_hole = |p: &Path| holes.iter().any(|h| p.starts_with(h) && p != h.as_path());
    let (walked, not_read) = of_walk(root);
    let strip = |e: Entries| -> Entries { e.into_iter().filter(|(p, _)| !under_hole(p)).collect() };
    let ours = strip(of_index(index));
    let theirs = strip(walked);
    if ours != theirs {
        let only_ours: Vec<_> = ours.keys().filter(|k| !theirs.contains_key(*k)).collect();
        let only_theirs: Vec<_> = theirs.keys().filter(|k| !ours.contains_key(*k)).collect();
        let differ: Vec<_> = ours
            .iter()
            .filter(|(k, v)| theirs.get(*k).is_some_and(|t| t != *v))
            .map(|(k, _)| k)
            .collect();
        panic!(
            "{context}: the index and the walk differ outside the holes {holes:?}\n\
             only in the index: {only_ours:?}\nonly in the walk: {only_theirs:?}\n\
             differing: {differ:?}"
        );
    }
    for folder in &not_read {
        assert!(
            holes.iter().any(|h| folder.starts_with(h)),
            "{context}: {} could not be read, and is claimed covered",
            folder.display()
        );
    }
}

fn open(s: &Scratch) -> Root {
    Root::open(s.path(), Exclusions::none()).unwrap()
}

fn opts() -> Options {
    Options {
        threads: 3,
        // SKWIRD_TEST_TICK_ZERO: no folder read again for being read
        // within a tick of its change, so that the check's comparison of
        // change times is all that finds what changed. Sound on kernels
        // whose change times are fine once observed (Linux 6.13).
        tick: if std::env::var_os("SKWIRD_TEST_TICK_ZERO").is_some() {
            std::time::Duration::ZERO
        } else {
            Options::default().tick
        },
        ..Options::default()
    }
}

fn limits() -> Limits {
    Limits {
        target: 40,
        max: 60,
        min: 15,
    }
}

fn first_index(root: &Root) -> Index {
    let start = Time::now();
    let crawl = crawl_all(root, &opts(), &Cancel::default()).unwrap();
    let mut index = build(crawl, limits());
    index.set_proof(Proof::Checked(start));
    index.set_crawled_at(start);
    index
}

fn refreshed(index: &mut Index, root: &mut Root) {
    refresh(index, root, &opts(), &Cancel::default()).unwrap();
}

fn hole(index: &Index, rel: &str) -> Option<Why> {
    index
        .coverage(index.root().unwrap())
        .holes
        .iter()
        .find(|h| h.path == Path::new(rel))
        .map(|h| h.why)
}

/// A change made while the engine was down: the index saved, the tree
/// changed, the index loaded is as of its save, then checked, and equals
/// the walk.
#[test]
fn a_change_made_while_the_engine_was_down_is_found() {
    let s = Scratch::new("cover-down");
    s.file("a/one.txt");
    s.file("b/two.txt");
    let mut root = open(&s);
    let mut index = first_index(&root);
    let dir = s.join(".cache-index");
    let saved_at = Time::now();
    let crawled_at = index.crawled_at();
    store::save(
        &mut index,
        &dir,
        &store::Mark {
            saved_at,
            crawled_at,
            proven_at: saved_at,
            epoch: 0,
            log_seq: 0,
        },
    )
    .unwrap();
    drop(index);
    s.file("a/three.txt");
    s.remove("b");
    s.file("c/d/four.txt");
    let (mut index, saved) = store::load(&dir, limits()).unwrap();
    index.set_proof(Proof::AsOf(saved.saved_at));
    assert_eq!(
        index.coverage(index.root().unwrap()).names,
        Proof::AsOf(saved_at)
    );
    refreshed(&mut index, &mut root);
    // The index's own folder is the walk's too: it was made after the
    // crawl, and the refresh crawled it.
    assert_sound(&index, s.path(), "after the restart");
}

/// A folder that gains entries, then is removed or replaced by a file:
/// nothing under it remains.
#[test]
fn a_folder_removed_or_replaced_leaves_nothing_under_it() {
    let s = Scratch::new("cover-removed");
    s.file("gone/x");
    s.file("swapped/y");
    let mut root = open(&s);
    let mut index = first_index(&root);
    s.file("gone/deeper/z");
    s.file("swapped/w");
    refreshed(&mut index, &mut root);
    assert_sound(&index, s.path(), "after gaining entries");
    s.remove("gone");
    s.remove("swapped");
    s.file("swapped");
    refreshed(&mut index, &mut root);
    assert_sound(&index, s.path(), "after removal");
    let entries = of_index(&index);
    assert!(!entries.keys().any(|p| p.starts_with("gone")));
    assert_eq!(entries[Path::new("swapped")].0, Kind::File);
    assert_eq!(
        entries.keys().filter(|p| p.starts_with("swapped")).count(),
        1
    );
}

/// A folder denied keeps what was read of it, and is a hole; made
/// readable, the next refresh reads it.
#[test]
fn a_denied_folder_is_a_hole_until_it_can_be_read() {
    let s = Scratch::new("cover-denied");
    s.file("shut/old.txt");
    let mut root = open(&s);
    let mut index = first_index(&root);
    s.file("shut/new.txt");
    s.deny("shut");
    if std::fs::read_dir(s.join("shut")).is_ok() {
        eprintln!("skipped: permissions do not hold here (root?)");
        return;
    }
    refreshed(&mut index, &mut root);
    assert_eq!(hole(&index, "shut"), Some(Why::Denied));
    assert!(
        of_index(&index).contains_key(Path::new("shut/old.txt")),
        "kept as read"
    );
    assert_sound(&index, s.path(), "denied");
    s.allow("shut");
    refreshed(&mut index, &mut root);
    assert_eq!(hole(&index, "shut"), None);
    assert_sound(&index, s.path(), "allowed again");
}

/// A folder made private is let go of at once, and is a hole; private no
/// more, it is read again.
#[test]
fn a_folder_made_private_is_let_go_of() {
    let s = Scratch::new("cover-private");
    s.file("diary/entry.txt");
    let mut root = open(&s);
    let mut index = first_index(&root);
    s.file("diary/.private");
    refreshed(&mut index, &mut root);
    assert_eq!(hole(&index, "diary"), Some(Why::Private));
    assert!(
        !of_index(&index)
            .keys()
            .any(|p| p.starts_with("diary") && p != Path::new("diary")),
        "nothing of it is kept"
    );
    assert_sound(&index, s.path(), "private");
    s.remove("diary/.private");
    refreshed(&mut index, &mut root);
    assert_eq!(hole(&index, "diary"), None);
    assert_sound(&index, s.path(), "private no more");
}

/// A listing that fails part-way keeps what the index held, as it was
/// read, and is a hole.
#[test]
fn a_listing_cut_short_keeps_what_was_read() {
    let s = Scratch::new("cover-cut");
    s.file("folder/a");
    s.file("folder/b/c");
    let root = open(&s);
    let mut index = first_index(&root);
    let folder = index.folder_at(Path::new("folder")).unwrap();
    let before = of_index(&index);
    let fresh = index
        .reread(folder, None, Seen::NotCovered(Why::CutShort(5)))
        .fresh;
    assert!(fresh.is_empty(), "{fresh:?}");
    assert_eq!(of_index(&index), before, "nothing let go of");
    assert_eq!(hole(&index, "folder"), Some(Why::CutShort(5)));
}

/// A mount made below the root is a hole, its folder's names let go of;
/// gone, the folder is read again.
#[test]
fn a_mount_made_below_the_root_is_a_hole() {
    if !skwird_testkit::in_namespace() {
        return;
    }
    let s = Scratch::new("cover-mount");
    s.file("m/under.txt");
    let mut root = open(&s);
    let mut index = first_index(&root);
    let at = s.mount_tmpfs("m");
    s.file("m/on-the-mount.txt");
    refreshed(&mut index, &mut root);
    assert_eq!(hole(&index, "m"), Some(Why::Mount));
    assert!(
        !of_index(&index)
            .keys()
            .any(|p| p.starts_with("m") && p != Path::new("m"))
    );
    std::process::Command::new("umount")
        .arg(&at)
        .status()
        .unwrap();
    refreshed(&mut index, &mut root);
    assert_eq!(hole(&index, "m"), None);
    assert!(of_index(&index).contains_key(Path::new("m/under.txt")));
}

/// splitmix64: the same run for the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One random edit of the tree, said in words for the failure message.
fn edit(s: &Scratch, rng: &mut Rng, n: &mut u32) -> String {
    // What is denied is what the walk cannot read, wherever a move took it.
    let (walked, denied) = of_walk(s.path());
    let allowed = |p: &Path| !denied.iter().any(|d| p.starts_with(d));
    let folders: Vec<PathBuf> = std::iter::once(PathBuf::new())
        .chain(
            walked
                .iter()
                .filter(|(p, (k, _))| *k == Kind::Folder && allowed(p))
                .map(|(p, _)| p.clone()),
        )
        .collect();
    let files: Vec<PathBuf> = walked
        .iter()
        .filter(|(p, (k, _))| *k != Kind::Folder && allowed(p))
        .map(|(p, _)| p.clone())
        .collect();
    let folder = folders[rng.below(folders.len())].clone();
    *n += 1;
    let new = |n: u32, what: &str| format!("{what} {n} é");
    match rng.below(12) {
        0 | 1 => {
            let path = folder.join(new(*n, "file"));
            s.file(&path);
            format!("made {}", path.display())
        }
        2 | 3 => {
            let path = folder.join(new(*n, "folder"));
            s.file(path.join("inside"));
            if rng.below(2) == 0 {
                s.file(path.join("deeper").join("more"));
            }
            format!("made {}", path.display())
        }
        4 if !files.is_empty() => {
            let file = &files[rng.below(files.len())];
            s.remove(file);
            format!("removed {}", file.display())
        }
        5 if folders.len() > 1 => {
            let gone = &folders[1 + rng.below(folders.len() - 1)];
            s.remove(gone);
            format!("removed {}", gone.display())
        }
        6 if folders.len() > 1 => {
            // A folder, or a file, moved to another folder not below it.
            let from = if rng.below(2) == 0 && !files.is_empty() {
                files[rng.below(files.len())].clone()
            } else {
                folders[1 + rng.below(folders.len() - 1)].clone()
            };
            let to = folder.join(new(*n, "moved"));
            if to.starts_with(&from) {
                return "no move".into();
            }
            s.rename(&from, &to);
            format!("moved {} to {}", from.display(), to.display())
        }
        7 if folders.len() > 2 => {
            // A folder swapped for a link to another.
            let swapped = folders[1 + rng.below(folders.len() - 1)].clone();
            let target = folders[rng.below(folders.len())].clone();
            s.remove(&swapped);
            s.link(&swapped, s.join(&target));
            format!(
                "swapped {} for a link to {}",
                swapped.display(),
                target.display()
            )
        }
        8 if folders.len() > 1 => {
            let shut = folders[1 + rng.below(folders.len() - 1)].clone();
            s.deny(&shut);
            format!("denied {}", shut.display())
        }
        9 if !denied.is_empty() => {
            let open = denied.iter().next().unwrap().clone();
            s.allow(&open);
            format!("allowed {}", open.display())
        }
        10 => {
            let marker = folder.join(".private");
            if s.join(&marker).exists() {
                s.remove(&marker);
                format!("took .private from {}", folder.display())
            } else {
                s.file(&marker);
                format!("made {} private", folder.display())
            }
        }
        _ => {
            let path = folder.join(new(*n, "file"));
            s.write(&path, b"some bytes");
            format!("made {}", path.display())
        }
    }
}

/// Random edits, made one at a time or several while the engine is
/// down, sometimes across a save and a load: after each refresh, the index
/// equals the walk wherever it claims to cover.
#[test]
fn random_edits_never_overstate_coverage() {
    // SKWIRD_TEST_SEEDS runs more: a longer search for a counterexample.
    let seeds: u64 = std::env::var("SKWIRD_TEST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    // The reasons the holes gave: the edits must make some of each kind
    // the test can make, or it judges nothing.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for seed in 1..=seeds {
        let s = Scratch::new("cover-random");
        if seed == 1 {
            s.dir("probe");
            s.deny("probe");
            let shut = std::fs::read_dir(s.join("probe")).is_err();
            s.allow("probe");
            s.remove("probe");
            if !shut {
                eprintln!("skipped: permissions do not hold here (root?)");
                return;
            }
        }
        for i in 0..6 {
            s.file(format!("start/f{i}/file"));
        }
        let mut root = open(&s);
        let mut index = first_index(&root);
        // A scratch folder of its own, removed even when the test fails.
        let store = Scratch::new("cover-random-index");
        let dir = store.join("index");
        let mut rng = Rng(seed);
        let mut n = 0;
        let mut done = Vec::new();
        for step in 0..60 {
            for _ in 0..=rng.below(3) {
                done.push(edit(&s, &mut rng, &mut n));
            }
            if rng.below(6) == 0 {
                let crawled_at = index.crawled_at();
                let now = Time::now();
                store::save(
                    &mut index,
                    &dir,
                    &store::Mark {
                        saved_at: now,
                        crawled_at,
                        proven_at: now,
                        epoch: 0,
                        log_seq: 0,
                    },
                )
                .unwrap();
                done.push("saved and loaded".into());
                (index, _) = store::load(&dir, limits()).unwrap();
            }
            refreshed(&mut index, &mut root);
            store::check(&index)
                .unwrap_or_else(|e| panic!("seed {seed}, step {step}: {e}: {done:?}"));
            assert_sound(
                &index,
                s.path(),
                &format!("seed {seed}, step {step}: {done:?}"),
            );
            for h in index.coverage(index.root().unwrap()).holes {
                seen.insert(format!("{:?}", h.why));
            }
        }
    }
    assert!(
        seen.contains("Denied") && seen.contains("Private"),
        "the edits made no hole of each kind: {seen:?}"
    );
}
