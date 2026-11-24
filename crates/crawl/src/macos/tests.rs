//! What macOS' crawl alone does (the shared tests are in `walk/tests.rs`):
//! a listing's attributes, which no stat gave, are a stat's; a folder of
//! more entries than one batch holds is listed whole; the mount table is
//! the system's; a volume mounted below a root is not crossed, whether
//! the table holds it or not.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use skwird_testkit::Scratch;

use crate::mounts::Table;
use crate::walk::tests::{listed, not_covered, open, opts};
use crate::{Cancel, Exclusions, Kind, Outcome, Root, Time, Why, crawl_all, read_folder};

fn time(sec: i64, nsec: i64) -> Time {
    Time {
        sec,
        nsec: u32::try_from(nsec).unwrap(),
    }
}

/// Each entry's attributes, as its folder's listing gave them, are what a
/// stat of it says: size, allocation, times, count of names.
#[test]
fn a_listings_attributes_are_a_stats() {
    let s = Scratch::new("macos-attrs");
    s.write("data.bin", &vec![7; 70_000]);
    s.file("empty");
    s.hard_link("second", "data.bin");
    s.link("link", "data.bin");
    s.dir("folder");
    let root = open(&s);
    let crawl = crawl_all(&root, &opts(1), &Cancel::default()).unwrap();
    let Outcome::Listed(listing) = &crawl.folders[0].outcome else {
        panic!("the root is listed");
    };
    assert_eq!(listing.len(), 5);
    for (name, entry) in listing.iter() {
        let name = std::str::from_utf8(name).unwrap();
        let m = std::fs::symlink_metadata(s.join(name)).unwrap();
        let attrs = entry
            .attrs
            .unwrap_or_else(|| panic!("{name}: no attributes"));
        assert_eq!(entry.id.ino, m.ino(), "{name}");
        assert_eq!(entry.id.dev, u64::from(m.dev() as u32), "{name}");
        assert_eq!(attrs.mtime, time(m.mtime(), m.mtime_nsec()), "{name}");
        assert_eq!(attrs.ctime, time(m.ctime(), m.ctime_nsec()), "{name}");
        if entry.kind == Kind::Folder {
            continue;
        }
        assert_eq!(attrs.size, m.size(), "{name}");
        assert_eq!(attrs.alloc, m.blocks() * 512, "{name}");
        assert_eq!(u64::from(attrs.links), m.nlink(), "{name}");
    }
    let kind = |n: &str| {
        listing
            .iter()
            .find(|(name, _)| *name == n.as_bytes())
            .unwrap()
            .1
            .kind
    };
    assert_eq!(kind("link"), Kind::Link);
    assert_eq!(kind("folder"), Kind::Folder);
    assert_eq!(kind("second"), Kind::File);
}

/// A folder of more entries than one batch holds (64 KiB, about 400 of
/// these) is listed whole, in the order of its names' bytes.
#[test]
fn a_folder_longer_than_a_batch_is_listed_whole() {
    let s = Scratch::new("macos-batches");
    let names: Vec<String> = (0..3000)
        .map(|i| format!("a name long enough that few fit in a batch, number {i:05}"))
        .collect();
    for name in &names {
        s.file(Path::new("many").join(name));
    }
    let root = open(&s);
    let (_, outcome) = read_folder(&root, Path::new("many"), &opts(1));
    let Outcome::Listed(listing) = outcome else {
        panic!("{outcome:?}");
    };
    let got: Vec<&[u8]> = listing.iter().map(|(n, _)| n).collect();
    let mut want: Vec<&[u8]> = names.iter().map(String::as_bytes).collect();
    want.sort_unstable();
    assert_eq!(got, want);
}

/// The mount table is the system's: `/` is in it, on this machine, and
/// the scratch folder's volume holds it.
#[test]
fn the_mount_table_is_the_systems() {
    let s = Scratch::new("macos-mounts");
    let table = Table::read().unwrap();
    let top = table
        .mounts()
        .iter()
        .find(|m| m.point == Path::new("/"))
        .expect("/ is mounted");
    assert!(top.local && !top.network(), "{top:?}");
    let holding = table
        .holding(s.path())
        .expect("a volume holds the scratch folder");
    assert!(!holding.network(), "{holding:?}");
    let root = open(&s);
    assert_eq!(root.fs_name(), holding.fstype);
}

/// A disk image attached at a folder, detached as the test ends.
struct Attached<'a> {
    at: &'a Path,
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(self.at)
            .status();
    }
}

/// A disk image made and attached at `rel` (`hdiutil`, which a user may
/// run): a volume of its own there.
fn attach<'a>(s: &Scratch, image: &Path, at: &'a Path) -> Attached<'a> {
    let made = Command::new("hdiutil")
        .args(["create", "-quiet", "-size", "8m", "-fs", "APFS", "-volname"])
        .arg("skwird-test")
        .arg(image)
        .status()
        .unwrap();
    assert!(made.success(), "hdiutil create");
    std::fs::create_dir_all(at).unwrap();
    let attached = Command::new("hdiutil")
        .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
        .arg(at)
        .arg(image)
        .status()
        .unwrap();
    assert!(attached.success(), "hdiutil attach");
    let _ = s;
    Attached { at }
}

/// A volume mounted below a root is not crossed: told by the mount table
/// where it holds it, and by the folder's device where the table was read
/// before it was mounted. Neither its names nor its folder's are listed.
#[test]
fn a_volume_mounted_below_a_root_is_not_crossed() {
    let s = Scratch::new("macos-volume");
    s.file("tree/mine.txt");
    let at = s.dir("tree/mounted");
    let image = s.join("volume.dmg");
    // Opened before the volume is mounted: its table does not hold it.
    let before = Root::open(&s.join("tree"), Exclusions::none()).unwrap();
    let _attached = attach(&s, &image, &at);
    std::fs::write(at.join("theirs.txt"), b"x").unwrap();
    let after = Root::open(&s.join("tree"), Exclusions::none()).unwrap();
    assert_eq!(
        after.mounts_below(),
        vec![Path::new("mounted").to_path_buf()]
    );
    for root in [&before, &after] {
        let crawl = crawl_all(root, &opts(2), &Cancel::default()).unwrap();
        let not = not_covered(&crawl);
        assert_eq!(not.get(Path::new("mounted")), Some(&Why::Mount), "{not:?}");
        let list = listed(&crawl);
        assert!(list.contains_key(Path::new("mine.txt")));
        assert!(list.contains_key(Path::new("mounted")));
        assert!(
            !list
                .keys()
                .any(|p| p.starts_with("mounted") && p != Path::new("mounted")),
            "{list:?}"
        );
    }
}

/// What `watcher` hears until `until` is heard, or 10 s pass.
fn heard_until(
    watcher: &std::sync::Arc<crate::Watcher>,
    until: impl Fn(&crate::Heard) -> bool + Send + 'static,
) -> Vec<crate::Heard> {
    let (tx, rx) = std::sync::mpsc::channel();
    let w = std::sync::Arc::clone(watcher);
    std::thread::spawn(move || {
        loop {
            let mut heard = Vec::new();
            if w.read(&mut heard).is_err() {
                return;
            }
            let done = heard.iter().any(&until);
            if tx.send(heard).is_err() || done {
                return;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut all = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(more) => all.extend(more),
            Err(_) => break,
        }
    }
    all
}

/// The roots' stream names each folder whose entries changed, below its
/// root, and a cookie made after a change is heard after it, in the same
/// stream.
#[test]
fn the_stream_names_each_folder_changed_and_a_cookie_in_its_turn() {
    use crate::Heard;
    let s = Scratch::new("macos-stream");
    s.dir("root/a/b");
    let cookies = s.dir("cookies");
    let root = crate::canonical(s.join("root")).unwrap();
    let watcher = std::sync::Arc::new(crate::Watcher::new(1, 0).unwrap());
    watcher.watch_root(0, &root).unwrap();
    watcher.watch_cookies(&cookies).unwrap();
    s.file("root/a/b/new.txt");
    let cookie = crate::cookie_folder(&cookies, 7);
    std::fs::create_dir(&cookie).unwrap();
    std::fs::write(cookie.join("made"), b"").unwrap();
    watcher.flush();
    let heard = heard_until(&watcher, |h| *h == Heard::Cookie(7));
    let changed = heard.iter().position(|h| {
        *h == Heard::Folder {
            scope: 0,
            rel: "a/b".into(),
        }
    });
    let cookie = heard.iter().position(|h| *h == Heard::Cookie(7));
    assert!(
        matches!((changed, cookie), (Some(c), Some(k)) if c < k),
        "{heard:?}"
    );
}

/// A root's own folder moved away is said: it is no longer heard.
#[test]
fn a_root_moved_away_is_lost() {
    use crate::Heard;
    let s = Scratch::new("macos-lost");
    s.dir("root");
    let root = crate::canonical(s.join("root")).unwrap();
    let watcher = std::sync::Arc::new(crate::Watcher::new(1, 0).unwrap());
    watcher.watch_root(3, &root).unwrap();
    s.rename("root", "moved");
    let heard = heard_until(&watcher, |h| *h == Heard::Lost { scope: 3 });
    assert!(heard.contains(&Heard::Lost { scope: 3 }), "{heard:?}");
}

/// A root's position, said as it is watched, replays on a stream of its
/// own what changed after it, then its end; a position of another event
/// database is refused, as one ahead of every event is.
#[test]
fn a_position_replays_what_changed_after_it() {
    use crate::Heard;
    let s = Scratch::new("macos-replay");
    s.dir("root/a");
    let root = crate::canonical(s.join("root")).unwrap();
    let watcher = std::sync::Arc::new(crate::Watcher::new(1, 0).unwrap());
    watcher.watch_root(0, &root).unwrap();
    let mut heard = Vec::new();
    watcher.read(&mut heard).unwrap();
    let Some(Heard::Position { at, .. }) = heard
        .iter()
        .find(|h| matches!(h, Heard::Position { scope: 0, .. }))
        .cloned()
    else {
        panic!("no position said: {heard:?}");
    };
    drop(watcher);
    s.file("root/a/made while none heard.txt");
    // Given time to be numbered and kept by the system.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let watcher = std::sync::Arc::new(crate::Watcher::new(2, 0).unwrap());
    let other = crate::Position {
        database: at.database ^ 1,
        id: at.id,
    };
    assert!(
        !watcher.replay_root(0, &root, other).unwrap(),
        "another database"
    );
    let ahead = crate::Position {
        database: at.database,
        id: u64::MAX - 1,
    };
    assert!(
        !watcher.replay_root(0, &root, ahead).unwrap(),
        "ahead of every event"
    );
    assert!(watcher.replay_root(0, &root, at).unwrap());
    let heard = heard_until(&watcher, |h| *h == Heard::Replayed { scope: 0 });
    assert!(
        heard.contains(&Heard::Folder {
            scope: 0,
            rel: "a".into()
        }),
        "{heard:?}"
    );
    assert!(heard.contains(&Heard::Replayed { scope: 0 }), "{heard:?}");
}

/// A folder with an ACL (an extended one, macOS'), its mode says so where
/// ACLs are asked, so that the system service never judges it by its mode
/// alone; one with none says none.
#[test]
fn a_folder_with_an_acl_says_so() {
    let s = Scratch::new("macos-acl");
    s.file("shared/inside");
    s.file("plain/inside");
    let added = Command::new("chmod")
        .args(["+a", "everyone allow list"])
        .arg(s.join("shared"))
        .status()
        .unwrap();
    assert!(added.success(), "chmod +a");
    let root = open(&s);
    let crawl = crawl_all(
        &root,
        &crate::Options {
            acls: true,
            ..opts(1)
        },
        &Cancel::default(),
    )
    .unwrap();
    let mode = |rel: &str| {
        crawl
            .folders
            .iter()
            .find(|f| f.rel == Path::new(rel))
            .and_then(|f| f.stat)
            .map(|s| s.mode)
            .unwrap()
    };
    assert_ne!(mode("shared") & crate::ACL, 0, "an ACL");
    assert_eq!(mode("plain") & crate::ACL, 0, "none");
}
