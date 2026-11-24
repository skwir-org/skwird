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
