//! What macOS' crawl alone does (the shared tests are in `walk/tests.rs`):
//! a listing's attributes, which no stat gave, are a stat's; a folder of
//! more entries than one batch holds is listed whole; the mount table is
//! the system's, the Data volume's firmlinks its mounts; a volume mounted
//! below a root is not crossed, whether the table holds it or not, and
//! what changes on it is its own root's alone; a change made through the
//! Data volume's mount is heard as users name it.

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
    assert!(
        watcher.replay_root(0, &root, at).unwrap(),
        "{}",
        crate::macos::watch::why_refused(&root, at)
    );
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

/// The Data volume's folders, where the system links them into `/`
/// (firmlinks), are mounts of the table, of the Data volume's device, as a
/// stat of them says; a crawl of `/` stops at each, as at a mount.
#[test]
fn a_firmlink_is_a_mount_of_the_data_volume() {
    let table = Table::read().unwrap();
    let data = table.data_volume().expect("the Data volume is mounted");
    let users = table
        .firmlinks()
        .find(|m| m.point == Path::new("/Users"))
        .expect("/Users is a firmlink");
    assert_eq!(users.root, Path::new("/Users"));
    assert_eq!(users.dev, data.dev);
    let dev = std::fs::metadata("/Users").unwrap().dev();
    // Its 32 bits, which std sign-extends.
    assert_eq!(dev & 0xFFFF_FFFF, data.id);
    assert_eq!(
        table.holding(Path::new("/Users/someone")).unwrap().point,
        Path::new("/Users")
    );
    let top = Root::open(Path::new("/"), Exclusions::none()).unwrap();
    assert_eq!(top.mount_at(Path::new("Users")), Some(false));
    assert_eq!(top.mount_at(Path::new("System/Volumes/Data")), Some(false));
}

/// A change made through the Data volume's own mount
/// (`/System/Volumes/Data/Users/...`) is heard by the path users name it
/// by, its firmlink's.
#[test]
fn a_change_through_the_data_volume_is_heard_as_users_name_it() {
    use crate::Heard;
    let s = Scratch::new("macos-data-path");
    s.dir("root/a");
    let cookies = s.dir("cookies");
    let root = crate::canonical(s.join("root")).unwrap();
    assert!(root.starts_with("/Users"), "a scratch folder below a home");
    let via_data = Path::new(crate::mounts::DATA_VOLUME).join(root.strip_prefix("/").unwrap());
    assert!(via_data.join("a").is_dir(), "{}", via_data.display());
    let watcher = std::sync::Arc::new(crate::Watcher::new(1, 0).unwrap());
    watcher.watch_root(0, &root).unwrap();
    watcher.watch_cookies(&cookies).unwrap();
    std::fs::write(via_data.join("a/new.txt"), b"x").unwrap();
    let cookie = crate::cookie_folder(&cookies, 3);
    std::fs::create_dir(&cookie).unwrap();
    std::fs::write(cookie.join("made"), b"").unwrap();
    let heard = heard_until(&watcher, |h| *h == Heard::Cookie(3));
    assert!(
        heard.contains(&Heard::Folder {
            scope: 0,
            rel: "a".into(),
        }),
        "{heard:?}"
    );
}

/// A change on a volume mounted below a root, itself a root, is that
/// root's alone: the outer root's crawl stops at the volume, and an
/// overflow there is not its own (as `/` above `/Users`). The inner root
/// is watched first: the volume's mount, which the system tells a while
/// after it is attached, would lower the outer root on a stream of the
/// outer root alone, as a volume mounted below a root does, and the
/// stream's 50 ms latency gives it there.
#[test]
fn a_change_on_a_volume_below_a_root_is_its_own_roots_alone() {
    use crate::Heard;
    let s = Scratch::new("macos-inner");
    s.file("tree/mine.txt");
    let at = s.dir("tree/mounted");
    let image = s.join("volume.dmg");
    let _attached = attach(&s, &image, &at);
    let cookies = s.dir("cookies");
    let outer = crate::canonical(s.join("tree")).unwrap();
    let inner = crate::canonical(&at).unwrap();
    let watcher = std::sync::Arc::new(crate::Watcher::new(1, 0).unwrap());
    watcher.watch_root(1, &inner).unwrap();
    watcher.watch_root(0, &outer).unwrap();
    watcher.watch_cookies(&cookies).unwrap();
    std::fs::create_dir(inner.join("made")).unwrap();
    std::fs::write(outer.join("also.txt"), b"x").unwrap();
    let cookie = crate::cookie_folder(&cookies, 5);
    std::fs::create_dir(&cookie).unwrap();
    std::fs::write(cookie.join("made"), b"").unwrap();
    let heard = heard_until(&watcher, |h| *h == Heard::Cookie(5));
    assert!(
        heard.contains(&Heard::Folder {
            scope: 1,
            rel: "".into(),
        }),
        "{heard:?}"
    );
    assert!(
        heard.contains(&Heard::Folder {
            scope: 0,
            rel: "".into(),
        }),
        "{heard:?}"
    );
    assert!(
        !heard
            .iter()
            .any(|h| matches!(h, Heard::Folder { scope: 0, rel } if rel.starts_with("mounted"))),
        "{heard:?}"
    );
    assert!(!heard.contains(&Heard::Overflow { scope: 0 }), "{heard:?}");
}

/// A folder's extended ACL is read whole, its entries in the order the
/// system keeps them (a deny before an allow), each its kind and rights;
/// a folder with none has none read.
#[test]
fn a_folders_extended_acl_is_read_whole() {
    use crate::acl::{EVERYONE, LIST_DIRECTORY, SEARCH};
    let s = Scratch::new("macos-acl-read");
    s.file("shared/inside");
    s.file("plain/inside");
    for entry in ["group:staff allow list,search", "everyone deny delete"] {
        let added = Command::new("chmod")
            .args(["+a", entry])
            .arg(s.join("shared"))
            .status()
            .unwrap();
        assert!(added.success(), "chmod +a {entry}");
    }
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
    let id = |rel: &str| {
        crawl
            .folders
            .iter()
            .find(|f| f.rel == Path::new(rel))
            .and_then(|f| f.stat)
            .map(|s| s.id)
            .unwrap()
    };
    let acl = root
        .access_acl(Path::new("shared"), id("shared"))
        .expect("its ACL");
    let entries = acl.entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let (deny, allow) = (entries[0], entries[1]);
    assert_eq!(
        (deny.who, deny.flags & 0xf, deny.rights),
        (EVERYONE, 2, 1 << 4)
    );
    assert_ne!(allow.who, EVERYONE, "staff's");
    assert_eq!(
        (allow.flags & 0xf, allow.rights),
        (1, LIST_DIRECTORY | SEARCH)
    );
    assert_eq!(root.access_acl(Path::new("plain"), id("plain")), None);
}

/// At the system service, a folder macOS asks consent for is withheld
/// below each home decided as it started, and below a home made since in
/// its folder of homes; the home itself, and its other folders, are not.
/// A user's engine withholds below its own home alone.
#[test]
fn consent_is_withheld_below_any_home_of_the_service() {
    use std::path::PathBuf;
    let service = super::Withheld {
        folders: vec![PathBuf::from("/Users/alice/Desktop")],
        homes_in: Some(PathBuf::from("/Users")),
    };
    assert!(service.holds(Path::new("/Users/alice/Desktop/a")));
    assert!(
        service.holds(Path::new("/Users/bob/Documents")),
        "a home made since"
    );
    assert!(service.holds(Path::new("/Users/bob/Library/Mobile Documents/x")));
    assert!(!service.holds(Path::new("/Users/bob")));
    assert!(!service.holds(Path::new("/Users/bob/Projects")));
    assert!(!service.holds(Path::new("/Users/Desktop")), "not in a home");
    let engine = super::Withheld {
        folders: vec![PathBuf::from("/Users/alice/Desktop")],
        homes_in: None,
    };
    assert!(!engine.holds(Path::new("/Users/bob/Documents")));
}
