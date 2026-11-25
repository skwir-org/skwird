//! The log (`docs/PLAN.md`, phase 1, step 4): an index opened replays
//! its log; the journal says what changed since a token, and when it
//! cannot; a save that fails keeps the index and what is on disk; a purge
//! leaves no trace of a folder's names; a damaged index is built again;
//! and a process killed at any moment leaves an index that opens sound.

#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use skwird_crawl::{Cancel, Exclusions, Kind, Options, Root, Why};
use skwird_index::keeper::{Event, Keeper, Opened, Reconciled, TooOld};
use skwird_index::log::{Op, Token};
use skwird_index::store::{self, Position};
use skwird_index::tree::Limits;
use skwird_index::{Index, Proof, Query};
use skwird_testkit::{Scratch, reference_walk};

type Entries = BTreeMap<PathBuf, (Kind, u64)>;

fn of_index(index: &Index) -> Entries {
    index
        .query(index.root().unwrap(), &Query::default())
        .unwrap()
        .into_iter()
        .map(|h| (h.path, (h.kind, h.id.ino)))
        .collect()
}

/// What a walk finds, but the index's own folder.
fn of_walk(root: &Path) -> Entries {
    reference_walk(root)
        .entries
        .into_iter()
        .filter(|(p, _)| !p.starts_with("index"))
        .map(|(p, s)| {
            let kind = match s.kind {
                skwird_testkit::Kind::File => Kind::File,
                skwird_testkit::Kind::Folder => Kind::Folder,
                skwird_testkit::Kind::Link => Kind::Link,
                skwird_testkit::Kind::Other => Kind::Other,
            };
            (p, (kind, s.ino))
        })
        .collect()
}

fn ours(index: &Index) -> Entries {
    of_index(index)
        .into_iter()
        .filter(|(p, _)| !p.starts_with("index"))
        .collect()
}

fn opts() -> Options {
    Options {
        threads: 3,
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

fn keeper(s: &Scratch) -> (Keeper, Opened) {
    let root = Root::open(s.path(), Exclusions::none()).unwrap();
    Keeper::open(&s.join("index"), root, opts(), limits(), &Cancel::default()).unwrap()
}

/// Opened where none is saved, it is built and saved; dropped unsaved
/// after a refresh, it opens again with its log replayed, as of that
/// refresh, and equal to the tree.
#[test]
fn an_index_opened_again_replays_its_log() {
    let s = Scratch::new("keeper-replay");
    s.file("a/one.txt");
    let (mut k, opened) = keeper(&s);
    assert!(
        matches!(opened, Opened::Built { because: None }),
        "{opened:?}"
    );
    s.file("a/two.txt");
    s.file("b/c/three.txt");
    s.remove("a/one.txt");
    k.refresh(&Cancel::default()).unwrap();
    let Proof::Checked(proven) = k.index().proof() else {
        panic!("refreshed, it is checked")
    };
    drop(k);
    let (k, opened) = keeper(&s);
    let Opened::Loaded { replayed, .. } = opened else {
        panic!("{opened:?}")
    };
    assert!(
        replayed >= 3,
        "the refresh's records and its proof: {replayed}"
    );
    assert_eq!(k.index().proof(), Proof::AsOf(proven));
    assert_eq!(ours(k.index()), of_walk(s.path()));
}

/// A batch heard that lets go of a folder (`a/x`, removed), makes one
/// (`b/n`), then names the one gone: its number is not given to the one
/// made while the batch runs. It was, and the one made was read in the
/// gone one's place, logged at a path the index no longer held: the next
/// start's replay refused the log, and built the index again.
#[test]
fn a_folders_number_is_not_given_again_within_a_batch() {
    let s = Scratch::new("keeper-numbers");
    s.file("a/x/one.txt");
    s.dir("b");
    let (mut k, _) = keeper(&s);
    let at = |k: &Keeper, p: &str| k.index().folder_at(Path::new(p)).unwrap();
    let heard = [at(&k, "a"), at(&k, "b"), at(&k, "a/x")];
    s.remove("a/x");
    s.file("b/n/two.txt");
    k.heard(&heard, &Cancel::default()).unwrap();
    assert_eq!(ours(k.index()), of_walk(s.path()));
    drop(k);
    let (k, opened) = keeper(&s);
    assert!(matches!(opened, Opened::Loaded { .. }), "{opened:?}");
    assert_eq!(ours(k.index()), of_walk(s.path()));
}

/// The journal: what changed below a folder since a token, each with its
/// identity and kind, a removal with what it was; a token of another
/// epoch, or from before the log, is too old.
#[test]
fn the_journal_says_what_changed_since_a_token() {
    let s = Scratch::new("keeper-journal");
    s.file("keep/old.txt");
    s.file("other/x");
    let (mut k, _) = keeper(&s);
    let since = k.token().unwrap();
    let old_ino = of_walk(s.path())[Path::new("keep/old.txt")].1;
    s.remove("keep/old.txt");
    s.file("keep/new.txt");
    s.file("keep/made/inside.txt");
    s.file("other/y");
    k.refresh(&Cancel::default()).unwrap();
    let walked = of_walk(s.path());
    let (events, next) = k.changes(Path::new("keep"), since).unwrap();
    let entry = |path: &str, op: Op| {
        events.iter().find_map(|e| match e {
            Event::Entry {
                path: p,
                op: o,
                entry,
            } if p == Path::new(path) && *o == op => Some(*entry),
            _ => None,
        })
    };
    let added = entry("keep/new.txt", Op::Added).expect("the new file");
    assert_eq!(
        (added.kind, added.id.ino),
        walked[Path::new("keep/new.txt")]
    );
    let removed = entry("keep/old.txt", Op::Removed).expect("the old file");
    assert_eq!((removed.kind, removed.id.ino), (Kind::File, old_ino));
    assert!(entry("keep/made", Op::Added).is_some_and(|e| e.kind == Kind::Folder));
    assert!(
        entry("keep/made/inside.txt", Op::Added).is_some(),
        "below a folder made"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Entry { path, .. } if path.starts_with("other"))),
        "only below the folder asked"
    );
    let (again, same) = k.changes(Path::new(""), next).unwrap();
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(same, next);
    let other = Token {
        epoch: next.epoch ^ 1,
        seq: 0,
    };
    assert_eq!(k.changes(Path::new(""), other), Err(TooOld));
    let ahead = Token {
        seq: next.seq + 5,
        ..next
    };
    assert_eq!(k.changes(Path::new(""), ahead), Err(TooOld));
}

/// A refresh that fails part way logs what it did before: the journal
/// says it, and the index opened again holds it. It was left in the index
/// alone: the journal never told it, nor the next check, which found the
/// folder as the index held it, and the next start lost it. A check that
/// gives the root as replaced after a folder changed makes it fail there.
#[test]
fn a_refresh_cut_short_logs_what_it_did() {
    let s = Scratch::new("keeper-cut-short");
    s.file("a/one.txt");
    let (mut k, _) = keeper(&s);
    let since = k.token().unwrap();
    s.file("a/two.txt");
    let cut = k.refresh_by(&Cancel::default(), &|root, stored, opts, cancel| {
        let mut checked = skwird_crawl::check(root, stored, opts, cancel)?;
        let top = (0..stored.len())
            .find(|&i| stored.rel(i).as_os_str().is_empty())
            .unwrap();
        checked.stale.push((top, skwird_crawl::Stale::Replaced));
        Ok(checked)
    });
    assert!(cut.is_err(), "{cut:?}");
    let (events, _) = k.changes(Path::new(""), since).unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Entry { path, op: Op::Added, .. } if path == Path::new("a/two.txt")
        )),
        "{events:?}"
    );
    drop(k);
    let (k, opened) = keeper(&s);
    assert!(matches!(opened, Opened::Loaded { .. }), "{opened:?}");
    assert_eq!(ours(k.index()), of_walk(s.path()));
}

/// The folder asked for removed, or one above it: the journal says so,
/// from the folder above's record, the change that names the way to it
/// alone. It once said nothing: no record's path was below the folder.
#[test]
fn the_journal_says_the_folder_asked_for_is_gone() {
    let s = Scratch::new("keeper-journal-gone");
    s.file("top/asked/deep/one.txt");
    s.file("top/beside.txt");
    let (mut k, _) = keeper(&s);
    let since = k.token().unwrap();
    s.remove("top/asked");
    s.remove("top/beside.txt");
    k.refresh(&Cancel::default()).unwrap();
    let removed = |asked: &str| -> Vec<PathBuf> {
        let (events, _) = k.changes(Path::new(asked), since).unwrap();
        events
            .into_iter()
            .map(|e| match e {
                Event::Entry {
                    path,
                    op: Op::Removed,
                    ..
                } => path,
                other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(removed("top/asked"), [PathBuf::from("top/asked")]);
    assert_eq!(
        removed("top/asked/deep"),
        [PathBuf::from("top/asked")],
        "a folder above it removed"
    );
    assert!(
        removed("top/other").is_empty(),
        "of the folder above's changes, those on the way alone"
    );
    // A folder above it no longer read: not covered, from there.
    s.dir("top/asked");
    k.refresh(&Cancel::default()).unwrap();
    let since = k.token().unwrap();
    let mode = |m| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(s.join("top"), std::fs::Permissions::from_mode(m))
    };
    mode(0o000).unwrap();
    let refreshed = k.refresh(&Cancel::default());
    mode(0o755).unwrap();
    refreshed.unwrap();
    let (events, _) = k.changes(Path::new("top/asked"), since).unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::NotCovered { path, why: Why::Denied } if path == Path::new("top")
        )),
        "{events:?}"
    );
}

/// A save that fails leaves the index as it is, and what is on disk as it
/// was: opened from it, with its log, it is as current as the refresh.
#[test]
fn a_save_that_fails_keeps_the_index_and_its_as_of() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("keeper-save-fails");
    s.file("a/one.txt");
    let (mut k, _) = keeper(&s);
    let dir = s.join("index");
    let manifest = std::fs::read(dir.join("manifest")).unwrap();
    s.file("a/two.txt");
    k.refresh(&Cancel::default()).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let failed = k.save();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if failed.is_ok() {
        eprintln!("skipped: permissions do not hold here (root?)");
        return;
    }
    assert_eq!(
        ours(k.index()),
        of_walk(s.path()),
        "the index in memory, as it is"
    );
    assert_eq!(
        std::fs::read(dir.join("manifest")).unwrap(),
        manifest,
        "the manifest as it was"
    );
    drop(k);
    let (k, opened) = keeper(&s);
    assert!(matches!(opened, Opened::Loaded { .. }), "{opened:?}");
    assert_eq!(ours(k.index()), of_walk(s.path()), "the log replayed");
}

/// Purged, a folder's names are found nowhere on disk: not in the
/// chunks, not in the names' files, not in the log; and a token from
/// before the purge is too old.
#[test]
fn a_purge_leaves_no_trace_of_a_folders_names() {
    let s = Scratch::new("keeper-purge");
    s.file("secret/zqxj-unique-name.txt");
    s.file("secret/inner/zqxj-other-name.txt");
    s.file("open/plain.txt");
    let (mut k, _) = keeper(&s);
    s.file("secret/zqxj-logged-name.txt");
    k.refresh(&Cancel::default()).unwrap();
    k.save().unwrap();
    let before = k.token().unwrap();
    k.purge(Path::new("secret"), Why::Excluded).unwrap();
    let entries = ours(k.index());
    assert!(entries.contains_key(Path::new("secret")));
    assert!(
        !entries
            .keys()
            .any(|p| p.starts_with("secret") && p != Path::new("secret"))
    );
    for file in std::fs::read_dir(s.join("index")).unwrap() {
        let file = file.unwrap();
        let bytes = std::fs::read(file.path()).unwrap();
        assert!(
            !bytes.windows(4).any(|w| w == b"zqxj"),
            "{} keeps a purged name",
            file.file_name().to_string_lossy()
        );
    }
    assert_eq!(k.changes(Path::new(""), before), Err(TooOld));
    assert!(entries.contains_key(Path::new("open/plain.txt")));
}

/// A keeper of the scratch tree with `excluded` (paths below it) kept out.
fn keeper_without(s: &Scratch, excluded: &[&str]) -> (Keeper, Opened) {
    let (ex, refused) = Exclusions::resolve(excluded.iter().map(|rel| s.join(rel)));
    assert!(refused.is_empty(), "{refused:?}");
    let root = Root::open(s.path(), ex).unwrap();
    Keeper::open(&s.join("index"), root, opts(), limits(), &Cancel::default()).unwrap()
}

fn reconciled(opened: &Opened) -> Reconciled {
    match opened {
        Opened::Loaded { reconciled, .. } => *reconciled,
        Opened::Built { .. } => panic!("{opened:?}"),
    }
}

/// The holes below the root, by path, with why.
fn holes(k: &Keeper) -> Vec<(PathBuf, Why)> {
    let index = k.index();
    index
        .coverage(index.root().unwrap())
        .holes
        .into_iter()
        .map(|h| (h.path, h.why))
        .filter(|(p, _)| !p.starts_with("index"))
        .collect()
}

/// Whether a file of the index's folder holds `needle`.
fn on_disk(s: &Scratch, needle: &[u8]) -> bool {
    std::fs::read_dir(s.join("index")).unwrap().any(|file| {
        let bytes = std::fs::read(file.unwrap().path()).unwrap();
        bytes.windows(needle.len()).any(|w| w == needle)
    })
}

/// An exclusion added while no engine ran (a line written in the user's
/// file, or a client's registration whose purge a kill cut short) holds
/// from the next open: the folder's names are purged before anything
/// reads the index, on disk too, though its parent never changed.
#[test]
fn an_exclusion_added_while_no_engine_ran_holds_from_the_next_open() {
    let s = Scratch::new("keeper-excluded-later");
    s.file("diary/zqxj-entry.txt");
    s.file("diary/inner/zqxj-deep.txt");
    s.file("open/plain.txt");
    let (k, _) = keeper(&s);
    assert!(on_disk(&s, b"zqxj"), "indexed while not excluded");
    drop(k);
    let (k, opened) = keeper_without(&s, &["diary"]);
    assert_eq!(
        reconciled(&opened),
        Reconciled {
            purged: 1,
            readmitted: 0,
            unsaved: false
        }
    );
    assert_eq!(holes(&k), [(PathBuf::from("diary"), Why::Excluded)]);
    let entries = ours(k.index());
    assert!(
        !entries
            .keys()
            .any(|p| p.starts_with("diary") && p != Path::new("diary")),
        "{entries:?}"
    );
    assert!(entries.contains_key(Path::new("open/plain.txt")));
    assert!(!on_disk(&s, b"zqxj"), "a purged name is kept on disk");
    // Opened again so, nothing is left to do.
    drop(k);
    let (_, opened) = keeper_without(&s, &["diary"]);
    assert_eq!(reconciled(&opened), Reconciled::default());
}

/// An exclusion taken back, while no engine ran or with a kill before
/// the folder was read again, is read at the next refresh: until then the
/// folder is not covered, not read yet, never excluded for good.
#[test]
fn an_exclusion_taken_back_is_read_again_even_across_a_kill() {
    let s = Scratch::new("keeper-readmitted");
    s.file("diary/entry.txt");
    s.file("diary/inner/deep.txt");
    s.file("open/plain.txt");
    let (k, _) = keeper_without(&s, &["diary"]);
    assert_eq!(holes(&k), [(PathBuf::from("diary"), Why::Excluded)]);
    drop(k);
    // Taken back while running, and killed before the refresh.
    let (mut k, opened) = keeper_without(&s, &["diary"]);
    assert_eq!(reconciled(&opened), Reconciled::default());
    let r = k.set_exclusions(Exclusions::none());
    assert_eq!(r.readmitted, 1, "{r:?}");
    assert_eq!(holes(&k), [(PathBuf::from("diary"), Why::Unread)]);
    drop(k);
    // Taken back while no engine ran: the next open finds it so.
    let (mut k, opened) = keeper(&s);
    assert_eq!(reconciled(&opened).readmitted, 1, "{opened:?}");
    assert_eq!(holes(&k), [(PathBuf::from("diary"), Why::Unread)]);
    k.refresh(&Cancel::default()).unwrap();
    assert_eq!(holes(&k), []);
    assert_eq!(ours(k.index()), of_walk(s.path()));
}

/// An index whose files are damaged is built again, and says why.
#[test]
fn a_damaged_index_is_built_again() {
    let s = Scratch::new("keeper-damaged");
    s.file("a/b");
    drop(keeper(&s));
    let chunk = std::fs::read_dir(s.join("index"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("chunk-")
        })
        .unwrap();
    std::fs::write(&chunk, b"not a chunk").unwrap();
    let (k, opened) = keeper(&s);
    assert!(
        matches!(opened, Opened::Built { because: Some(_) }),
        "{opened:?}"
    );
    assert_eq!(ours(k.index()), of_walk(s.path()));
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

/// The child's loop: edit the tree, refresh, save now and then, until
/// killed.
fn edit_until_killed(tree: &Path, seed: u64) -> ! {
    let root = Root::open(tree, Exclusions::none()).unwrap();
    let (mut k, _) = Keeper::open(
        &tree.join("index"),
        root,
        opts(),
        limits(),
        &Cancel::default(),
    )
    .unwrap();
    let mut rng = Rng(seed);
    let mut n = 0u64;
    loop {
        n += 1;
        let folders: Vec<PathBuf> = std::iter::once(PathBuf::new())
            .chain(
                of_walk(tree)
                    .into_iter()
                    .filter(|(_, (k, _))| *k == Kind::Folder)
                    .map(|(p, _)| p),
            )
            .collect();
        let folder = tree.join(&folders[rng.below(folders.len())]);
        match rng.below(4) {
            0 | 1 => {
                let _ = std::fs::write(folder.join(format!("file {n}")), b"x");
            }
            2 => {
                let _ = std::fs::create_dir_all(folder.join(format!("made {n}/inner")));
            }
            _ => {
                if folders.len() > 1 {
                    let _ = std::fs::remove_dir_all(
                        tree.join(&folders[1 + rng.below(folders.len() - 1)]),
                    );
                }
            }
        }
        let _ = k.refresh(&Cancel::default());
        if n % 3 == 0 {
            let _ = k.save();
        }
    }
}

/// Killed at any moment, mid-append or mid-save, a process leaves an
/// index that opens (loaded, or built again where what is on disk does
/// not read) and refreshes to equal the tree.
#[test]
fn a_kill_at_any_moment_leaves_an_index_that_opens_sound() {
    const CHILD: &str = "SKWIRD_TEST_KILL_TREE";
    if let Some(tree) = std::env::var_os(CHILD) {
        let seed = std::env::var("SKWIRD_TEST_KILL_SEED")
            .unwrap()
            .parse()
            .unwrap();
        edit_until_killed(Path::new(&tree), seed);
    }
    let s = Scratch::new("keeper-kill");
    for i in 0..5 {
        s.file(format!("start/f{i}/file"));
    }
    let current = std::thread::current();
    let test = current.name().unwrap();
    let mut rng = Rng(7);
    let mut loaded = 0;
    // SKWIRD_TEST_ROUNDS kills more often: a longer search.
    let rounds: u64 = std::env::var("SKWIRD_TEST_ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(12);
    for round in 0..rounds {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD, s.path())
            .env("SKWIRD_TEST_KILL_SEED", round.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(40 + rng.below(400) as u64));
        child.kill().unwrap();
        child.wait().unwrap();
        let (mut k, opened) = keeper(&s);
        match &opened {
            Opened::Loaded { .. } => loaded += 1,
            Opened::Built { because } => eprintln!("round {round}: built again, {because:?}"),
        }
        k.refresh(&Cancel::default()).unwrap();
        assert_eq!(
            ours(k.index()),
            of_walk(s.path()),
            "round {round}: {opened:?}"
        );
    }
    eprintln!("{loaded} of {rounds} rounds loaded what the killed process left");
    assert!(loaded > 0, "no round loaded what a killed process left");
}

/// A folder with an access ACL, kept for a system service, is answered
/// like any other once built, and once loaded again.
#[test]
fn a_folder_with_an_acl_is_kept_like_any() {
    let s = Scratch::new("keeper-acl");
    s.file("shared/inside.txt");
    s.file("plain/other.txt");
    let set = std::process::Command::new("setfacl")
        .args(["-m", "u:65534:rx"])
        .arg(s.join("shared"))
        .status();
    if !set.is_ok_and(|st| st.success()) {
        eprintln!("skipped: no setfacl, or no ACLs on this filesystem");
        return;
    }
    let acls = Options {
        acls: true,
        ..opts()
    };
    for _ in 0..2 {
        let root = Root::open(s.path(), Exclusions::none()).unwrap();
        let (k, _) = Keeper::open(
            &s.join("index"),
            root,
            acls.clone(),
            limits(),
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(ours(k.index()), of_walk(s.path()));
    }
}

/// A folder's ACL stays answered while other folders are applied and the
/// index saved, and follows its own change, heard as the system service
/// hears it (fanotify says a folder's own change of the folder itself):
/// the ACLs are read again only for the folders a read's records name,
/// and published again only where one changed (DESIGN 8, "An application
/// costs what it changed").
#[test]
fn a_folders_acl_is_kept_through_applications_and_saves() {
    let s = Scratch::new("keeper-acl-kept");
    s.file("shared/inside.txt");
    s.file("plain/other.txt");
    let facl = |args: &[&str]| {
        std::process::Command::new("setfacl")
            .args(args)
            .arg(s.join("shared"))
            .status()
            .is_ok_and(|st| st.success())
    };
    if !facl(&["-m", "u:65534:rx"]) {
        eprintln!("skipped: no setfacl, or no ACLs on this filesystem");
        return;
    }
    let acls = Options {
        acls: true,
        ..opts()
    };
    let root = Root::open(s.path(), Exclusions::none()).unwrap();
    let cancel = Cancel::default();
    let (mut k, _) = Keeper::open(&s.join("index"), root, acls, limits(), &cancel).unwrap();
    // As a query sees it: in the snapshot published.
    let has = |k: &Keeper| {
        let index = k.snapshot();
        let shared = index.folder_at(Path::new("shared")).unwrap();
        let facts = index.facts_of(shared).unwrap();
        (
            facts.mode & skwird_crawl::ACL != 0,
            index.acl_of(facts.id).is_some(),
        )
    };
    assert_eq!(has(&k), (true, true), "read once built");
    let plain = k.index().folder_at(Path::new("plain")).unwrap();
    for (i, save) in [(0, false), (1, true), (2, false)] {
        s.file(format!("plain/more-{i}.txt"));
        k.heard_named(&[(plain, None)], &cancel).unwrap();
        if save {
            k.save().unwrap();
        }
        assert_eq!(has(&k), (true, true), "round {i}, saved: {save}");
    }
    let shared = k.index().folder_at(Path::new("shared")).unwrap();
    assert!(facl(&["-b"]));
    k.heard_named(&[(shared, None)], &cancel).unwrap();
    assert_eq!(has(&k), (false, false), "its ACL taken away");
    assert!(facl(&["-m", "u:65534:rx"]));
    k.heard_named(&[(shared, None)], &cancel).unwrap();
    assert_eq!(has(&k), (true, true), "its ACL given again");
}

/// A save with nothing new since the last writes nothing, not even the
/// manifest (DESIGN 8, "A save writes what is new"): the service saves
/// every root at once, most of them unchanged. A change heard after is
/// saved, and read back.
#[test]
fn a_save_with_nothing_new_writes_nothing() {
    let s = Scratch::new("keeper-save-nothing");
    s.file("a/one");
    let (mut k, _) = keeper(&s);
    let manifest = s.join("index/manifest");
    let written = || std::fs::metadata(&manifest).unwrap().modified().unwrap();
    k.save().unwrap();
    let before = written();
    std::thread::sleep(std::time::Duration::from_millis(20));
    k.save().unwrap();
    assert_eq!(written(), before, "nothing new: nothing saved again");
    s.file("a/two");
    let a = k.index().folder_at(Path::new("a")).unwrap();
    k.heard_named(&[(a, None)], &Cancel::default()).unwrap();
    k.save().unwrap();
    assert!(written() > before, "a change saved");
    let after = written();
    std::thread::sleep(std::time::Duration::from_millis(20));
    k.save().unwrap();
    assert_eq!(written(), after, "nothing new since");
    drop(k);
    let (k, opened) = keeper(&s);
    assert!(
        matches!(opened, Opened::Loaded { replayed: 0, .. }),
        "{opened:?}"
    );
    assert_eq!(ours(k.index()), of_walk(s.path()));
}

/// The position a change history was read up to (DESIGN 2): set, it is
/// saved with the index though nothing else changed, and read back from
/// the manifest alone, before the index, and with it; taken back, it is
/// saved as none. A manifest of the version before it (3) still reads,
/// with none: no index is built again for it.
#[test]
fn a_position_is_saved_with_the_index_and_read_back() {
    let s = Scratch::new("keeper-position");
    s.file("a/one");
    let dir = s.join("index");
    let (mut k, _) = keeper(&s);
    assert_eq!(k.position(), None, "built: none read");
    let at = Position {
        history: 0x1234_5678,
        next: 4096,
    };
    k.set_position(Some(at));
    k.save().unwrap();
    drop(k);
    assert_eq!(store::read_mark(&dir).and_then(|m| m.position), Some(at));
    let (mut k, opened) = keeper(&s);
    assert!(matches!(opened, Opened::Loaded { .. }), "{opened:?}");
    assert_eq!(k.position(), Some(at));
    k.set_position(None);
    k.save().unwrap();
    drop(k);
    assert_eq!(store::read_mark(&dir).and_then(|m| m.position), None);
    // The same manifest as version 3 wrote it: its version, no position.
    let manifest = dir.join("manifest");
    let mut bytes = std::fs::read(&manifest).unwrap();
    // Its magic, three versions, the save's number, three times, the log's
    // epoch and last record: the position's tag after.
    let tag = 4 + 3 * 4 + 8 + 3 * 12 + 2 * 8;
    assert_eq!(bytes[tag], 0, "no position");
    bytes.remove(tag);
    bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
    std::fs::write(&manifest, &bytes).unwrap();
    let mark = store::read_mark(&dir).expect("version 3 still reads");
    assert_eq!(mark.position, None);
    let (k, opened) = keeper(&s);
    assert!(matches!(opened, Opened::Loaded { .. }), "{opened:?}");
    assert_eq!(ours(k.index()), of_walk(s.path()));
}

/// A file written to, heard by its name, is patched into its chunk, not
/// cut again (DESIGN 8): its new size is answered, a query on size
/// finding it by its zone, and it is saved and read back so, under its
/// chunk's new number. A file made splices it. That a patch
/// reads as a chunk made anew is `chunk`'s test; this one holds its place
/// in the keeper: the places it moves, the zones, the save.
#[test]
fn a_file_written_to_is_patched_saved_and_read_back() {
    let s = Scratch::new("keeper-patch");
    for i in 0..30 {
        s.file(format!("a/entry-{i:02}"));
        s.file(format!("b/entry-{i:02}"));
    }
    let (mut k, _) = keeper(&s);
    let sized = |index: &Index| -> Vec<PathBuf> {
        let query = Query {
            size: Some(4321..=4321),
            ..Query::default()
        };
        index
            .query(index.root().unwrap(), &query)
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect()
    };
    assert_eq!(sized(k.index()), Vec::<PathBuf>::new());
    let before = k.index().tree().chunk_ids().to_vec();
    std::fs::write(s.join("a/entry-07"), vec![b'x'; 4321]).unwrap();
    let folder = k.index().folder_at(Path::new("a")).unwrap();
    let names: HashSet<Box<[u8]>> = [Box::from(&b"entry-07"[..])].into();
    let done = k
        .heard_named(&[(folder, Some(names))], &Cancel::default())
        .unwrap();
    assert_eq!(
        (done.patched, done.spliced, done.cut),
        (1, 0, 0),
        "patched, not cut again"
    );
    assert_eq!(sized(k.index()), vec![PathBuf::from("a/entry-07")]);
    assert_eq!(
        k.index().tree().chunk_ids(),
        before.as_slice(),
        "no chunk cut again, none renumbered before the save"
    );
    k.save().unwrap();
    let after = k.index().tree().chunk_ids().to_vec();
    assert_eq!(
        after.iter().filter(|id| !before.contains(id)).count(),
        1,
        "the folder's chunk saved under a number of its own"
    );
    drop(k);
    let (k, opened) = keeper(&s);
    assert!(matches!(opened, Opened::Loaded { .. }), "{opened:?}");
    assert_eq!(sized(k.index()), vec![PathBuf::from("a/entry-07")]);
    assert_eq!(ours(k.index()), of_walk(s.path()));
    // A file made changes the folder's shape: its chunk's columns are
    // spliced.
    let mut k = k;
    s.file("a/entry-99");
    let folder = k.index().folder_at(Path::new("a")).unwrap();
    let names: HashSet<Box<[u8]>> = [Box::from(&b"entry-99"[..])].into();
    let done = k
        .heard_named(&[(folder, Some(names))], &Cancel::default())
        .unwrap();
    assert_eq!((done.patched, done.spliced, done.cut), (0, 1, 0));
    assert_eq!(ours(k.index()), of_walk(s.path()));
}
