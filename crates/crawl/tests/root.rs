// Root, fanotify and mounts are Linux's here; the other systems' come
// with their phases.
#![cfg(target_os = "linux")]

//! What the crawl does as root, in the test machine (`tools/vm/root.nix`):
//! each test is skipped anywhere else.

use std::os::unix::fs::MetadataExt;

use skwird_testkit::machine;

/// `statfs`'s word for ext4 and for btrfs.
const EXT4: u64 = 0xEF53;
const BTRFS: u64 = 0x9123_683E;

fn kind(path: &std::path::Path) -> u64 {
    rustix::fs::statfs(path).expect("cannot statfs").f_type as u64
}

/// The machine is what the tests below rely on: an ext4 filesystem, and
/// a btrfs one mounted through a subvolume, whose subvolumes each have a
/// device of their own (DESIGN 6).
#[test]
fn the_machine_has_its_filesystems() {
    let Some(m) = machine() else { return };
    assert_eq!(kind(&m.ext4), EXT4);
    assert_eq!(kind(&m.btrfs), BTRFS);
    let top = std::fs::metadata(&m.btrfs).unwrap().dev();
    let a = std::fs::metadata(m.btrfs.join("a")).unwrap().dev();
    let b = std::fs::metadata(m.btrfs.join("b")).unwrap().dev();
    assert!(top != a && a != b && top != b, "{top} {a} {b}");
    // Every subvolume's top folder is inode 256.
    assert_eq!(std::fs::metadata(m.btrfs.join("a")).unwrap().ino(), 256);
}

/// A root read lets go of its folder, so that its filesystem can be
/// unmounted between reads; read again, it is opened again; its mount gone,
/// it is not covered, never read as the folder under it.
#[test]
fn a_root_holds_its_filesystem_only_while_read() {
    use skwird_crawl::{Cancel, Exclusions, Options, Root, crawl_all};
    let Some(m) = machine() else { return };
    let point = m.ext4.join(format!("unmountable-{}", std::process::id()));
    std::fs::create_dir_all(&point).unwrap();
    let mount = || {
        rustix::mount::mount(
            "none",
            &point,
            "tmpfs",
            rustix::mount::MountFlags::empty(),
            None,
        )
    };
    let unmount = || rustix::mount::unmount(&point, rustix::mount::UnmountFlags::empty());
    mount().unwrap();
    std::fs::create_dir_all(point.join("inside")).unwrap();
    let root = Root::open(&point, Exclusions::default()).unwrap();
    let read = || crawl_all(&root, &Options::default(), &Cancel::default());
    assert_eq!(read().unwrap().folders.len(), 2);
    assert_eq!(unmount(), Err(rustix::io::Errno::BUSY), "held while read");
    root.let_go();
    assert_eq!(read().unwrap().folders.len(), 2, "opened again");
    root.let_go();
    unmount().unwrap();
    // Its mount gone, the folder there is another: the root is not read.
    let after = read().unwrap();
    assert_eq!(after.folders.len(), 1);
    assert!(
        matches!(
            after.folders[0].outcome,
            skwird_crawl::Outcome::NotCovered(_)
        ),
        "{:?}",
        after.folders[0].outcome
    );
    std::fs::remove_dir(&point).unwrap();
}

/// A btrfs snapshot below a root, a read-only subvolume, is left out, its
/// name kept; a writable subvolume beside it is read; the snapshot named
/// as a root is read (DESIGN, open question 10).
#[test]
fn a_snapshot_below_a_root_is_left_out() {
    use skwird_crawl::{Cancel, Exclusions, Options, Outcome, Root, Why, crawl_all};
    let Some(m) = machine() else { return };
    let place = skwird_testkit::Place::new(&m.btrfs, "snapshots");
    let btrfs = |args: &[&std::ffi::OsStr]| {
        let ok = std::process::Command::new("btrfs")
            .arg("-q")
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "btrfs {args:?}");
    };
    let live = place.join("live");
    let snapshot = place.join("snapshot");
    btrfs(&["subvolume".as_ref(), "create".as_ref(), live.as_os_str()]);
    std::fs::write(live.join("in the subvolume.txt"), b"x").unwrap();
    btrfs(&[
        "subvolume".as_ref(),
        "snapshot".as_ref(),
        "-r".as_ref(),
        live.as_os_str(),
        snapshot.as_os_str(),
    ]);
    let crawl = |at: &std::path::Path| {
        let root = Root::open(at, Exclusions::none()).unwrap();
        crawl_all(&root, &Options::default(), &Cancel::default()).unwrap()
    };
    let outcome = |c: &skwird_crawl::Crawl, rel: &str| {
        c.folders
            .iter()
            .find(|f| f.rel == std::path::Path::new(rel))
            .map(|f| matches!(f.outcome, Outcome::NotCovered(Why::Snapshot)))
    };
    let below = crawl(&place.0);
    assert_eq!(outcome(&below, "snapshot"), Some(true), "left out");
    assert_eq!(outcome(&below, "live"), Some(false), "read");
    let named = crawl(&snapshot);
    assert_eq!(outcome(&named, ""), Some(false), "named, read");
    assert_eq!(named.folders.len(), 1);
    btrfs(&[
        "subvolume".as_ref(),
        "delete".as_ref(),
        snapshot.as_os_str(),
    ]);
    btrfs(&["subvolume".as_ref(), "delete".as_ref(), live.as_os_str()]);
}

mod fanotify {
    // The machine, a group, the table and a resolver, in every test: short
    // names read best beside the closures' own.
    #![allow(clippy::many_single_char_names)]

    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use skwird_crawl::FileId;
    use skwird_crawl::fanotify::{Folder, Group, Heard, Refused, Resolved, Through};
    use skwird_crawl::mounts::Table;
    use skwird_testkit::{Place, machine};

    fn table() -> Table {
        Table::read().unwrap()
    }

    fn id(path: &Path) -> FileId {
        let m = std::fs::symlink_metadata(path).unwrap();
        FileId {
            dev: m.dev(),
            ino: m.ino(),
        }
    }

    /// What a folder's handle should resolve to: its path and identity.
    fn at(path: &Path) -> Resolved {
        Resolved::At(path.to_path_buf(), id(path))
    }

    /// What `g` hears until `done` holds of it, failing after 5 s.
    fn hear(g: &Group, done: impl Fn(&[Heard]) -> bool) -> Vec<Heard> {
        let mut heard = Vec::new();
        let start = Instant::now();
        loop {
            g.read(&mut heard).unwrap();
            if done(&heard) {
                return heard;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "not heard in 5 s, of {} events",
                heard.len()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The folders an entry of this name was heard in.
    fn heard_in<'a>(heard: &'a [Heard], name: &str) -> Vec<&'a Folder> {
        heard
            .iter()
            .filter_map(|h| match h {
                Heard::Entry(folder, n) if &**n == name.as_bytes() => Some(folder),
                _ => None,
            })
            .collect()
    }

    fn position(heard: &[Heard], name: &str) -> usize {
        heard
            .iter()
            .position(|h| matches!(h, Heard::Entry(_, n) if &**n == name.as_bytes()))
            .unwrap_or_else(|| panic!("{name} not heard"))
    }

    /// A change on ext4 is heard named by its folder's handle, which opens
    /// to that folder: names with a space, an accent, made, moved and
    /// removed, and the folders made, each in its own parent.
    #[test]
    fn a_change_is_heard_by_its_folders_handle() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "heard");
        let g = Group::new(1).unwrap();
        let marked = g.mark(&place.0, &table()).unwrap();
        assert_eq!((marked.through, marked.new), (Through::Itself, true));
        let deep = place.join("a b/deeper");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("one two.txt"), b"x").unwrap();
        std::fs::rename(deep.join("one two.txt"), deep.join("é.txt")).unwrap();
        std::fs::remove_file(deep.join("é.txt")).unwrap();
        let heard = hear(&g, |h| !heard_in(h, "é.txt").is_empty());
        let t = table();
        let mut r = g.resolver(&t);
        for (name, folder) in [
            ("a b", place.0.clone()),
            ("deeper", place.join("a b")),
            ("one two.txt", deep.clone()),
            ("é.txt", deep.clone()),
        ] {
            let folders = heard_in(&heard, name);
            assert!(!folders.is_empty(), "{name} not heard");
            for f in folders {
                assert_eq!(f.fsid, marked.fsid);
                assert_eq!(r.resolve(f), at(&folder), "{name}");
            }
        }
        // Marked once: a second ask says so.
        assert!(!g.mark(&deep, &table()).unwrap().new);
    }

    /// A btrfs mounted through a subvolume is marked through its top level,
    /// once for all its subvolumes, and heard in each, the mount made for
    /// the mark gone: each folder's handle opens to it, in its subvolume.
    #[test]
    fn btrfs_is_marked_through_its_top_level_for_every_subvolume() {
        let Some(m) = machine() else { return };
        let g = Group::new(1).unwrap();
        let marked = g.mark(&m.btrfs, &table()).unwrap();
        assert_eq!((marked.through, marked.new), (Through::TopLevel, true));
        let again = g.mark(&m.btrfs.join("a"), &table()).unwrap();
        assert_eq!((again.fsid, again.new), (marked.fsid, false));
        let places: Vec<Place> = ["", "a", "b"]
            .iter()
            .map(|sub| Place::new(&m.btrfs.join(sub), "subvolume"))
            .collect();
        for (i, p) in places.iter().enumerate() {
            std::fs::write(p.join(&format!("in-subvolume-{i}")), b"x").unwrap();
        }
        let names: Vec<String> = (0..3).map(|i| format!("in-subvolume-{i}")).collect();
        let heard = hear(&g, |h| names.iter().all(|n| !heard_in(h, n).is_empty()));
        let t = table();
        let mut r = g.resolver(&t);
        for (p, name) in places.iter().zip(&names) {
            for f in heard_in(&heard, name) {
                assert_eq!(f.fsid, marked.fsid, "the mark's id, whatever the subvolume");
                assert_eq!(r.resolve(f), at(&p.0), "{name}");
            }
        }
    }

    /// More events than the queue holds, unread, are heard as an overflow.
    #[test]
    fn an_overflow_is_heard() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "overflow");
        let g = Group::new(1).unwrap();
        g.mark(&place.0, &table()).unwrap();
        // The queue holds 16,384 events; distinct names are not merged.
        for i in 0..20_000 {
            std::fs::File::create(place.join(&format!("f{i}"))).unwrap();
        }
        hear(&g, |h| h.contains(&Heard::Overflow));
    }

    /// A folder emptied, then removed: its handle no longer opens, and its
    /// removal is heard in its parent, after what was heard in it.
    #[test]
    fn a_removed_folder_is_gone_and_heard_in_its_parent() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "removed");
        std::fs::create_dir_all(place.join("x-removed")).unwrap();
        std::fs::write(place.join("x-removed/f-in-removed"), b"x").unwrap();
        let g = Group::new(1).unwrap();
        g.mark(&place.0, &table()).unwrap();
        std::fs::remove_file(place.join("x-removed/f-in-removed")).unwrap();
        std::fs::remove_dir(place.join("x-removed")).unwrap();
        let heard = hear(&g, |h| !heard_in(h, "x-removed").is_empty());
        assert!(position(&heard, "f-in-removed") < position(&heard, "x-removed"));
        let t = table();
        let mut r = g.resolver(&t);
        for f in heard_in(&heard, "f-in-removed") {
            assert_eq!(r.resolve(f), Resolved::Gone);
        }
        for f in heard_in(&heard, "x-removed") {
            assert_eq!(r.resolve(f), at(&place.0));
        }
    }

    /// A folder moved is heard in its old parent and its new.
    #[test]
    fn a_folder_moved_is_heard_in_both_its_parents() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "moved");
        std::fs::create_dir_all(place.join("from/moving")).unwrap();
        std::fs::create_dir_all(place.join("to")).unwrap();
        let g = Group::new(1).unwrap();
        g.mark(&place.0, &table()).unwrap();
        std::fs::rename(place.join("from/moving"), place.join("to/moving")).unwrap();
        let heard = hear(&g, |h| heard_in(h, "moving").len() >= 2);
        let t = table();
        let mut r = g.resolver(&t);
        let parents: Vec<Resolved> = heard_in(&heard, "moving")
            .into_iter()
            .map(|f| r.resolve(f))
            .collect();
        assert!(parents.contains(&at(&place.join("from"))));
        assert!(parents.contains(&at(&place.join("to"))));
    }

    /// A folder's own attributes changed (its mode) are heard as the folder
    /// itself changed, by its own handle: read again, it is stat-ed first.
    #[test]
    fn a_folders_mode_changed_is_heard_as_the_folder_itself() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "mode");
        std::fs::create_dir_all(place.join("private-ish")).unwrap();
        let g = Group::new(1).unwrap();
        g.mark(&place.0, &table()).unwrap();
        std::fs::set_permissions(
            place.join("private-ish"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let heard = hear(&g, |h| {
            h.iter().any(|h| {
                matches!(h, Heard::Entry(_, n) if &**n == b"private-ish")
                    || matches!(h, Heard::Changed(_))
            })
        });
        let t = table();
        let mut r = g.resolver(&t);
        let said: Vec<String> = heard
            .iter()
            .filter_map(|h| match h {
                Heard::Entry(f, n) if &**n == b"private-ish" => {
                    Some(format!("entry in {:?}", r.resolve(f)))
                }
                Heard::Changed(f) => Some(format!("changed {:?}", r.resolve(f))),
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            [format!("changed {:?}", at(&place.join("private-ish")))],
            "the folder {:?}",
            id(&place.join("private-ish"))
        );
    }

    /// A cookie is heard after every event queued before it, those of
    /// another filesystem marked by the group included: one queue.
    #[test]
    fn a_cookie_comes_after_everything_before_it() {
        let Some(m) = machine() else { return };
        let cookies = Place::new(&m.ext4, "cookies");
        let other = Place::new(&m.btrfs.join("a"), "before-cookie");
        let g = Group::new(1).unwrap();
        g.mark(&cookies.0, &table()).unwrap();
        g.mark(&other.0, &table()).unwrap();
        g.watch_cookies(&cookies.0, &table()).unwrap();
        std::fs::write(other.join("made-before-the-cookie"), b"x").unwrap();
        std::fs::write(cookies.join("cookie-7"), b"").unwrap();
        let heard = hear(&g, |h| h.contains(&Heard::Cookie(7)));
        let cookie = heard.iter().position(|h| *h == Heard::Cookie(7)).unwrap();
        assert!(position(&heard, "made-before-the-cookie") < cookie);
    }

    /// A mount put over the filesystem's folder since the table was read is
    /// not taken for it: the handle fails to open, never opens to another.
    #[test]
    fn a_mount_over_the_filesystems_folder_is_not_taken_for_it() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "covered");
        let g = Group::new(1).unwrap();
        g.mark(&place.0, &table()).unwrap();
        std::fs::write(place.join("before-the-cover"), b"x").unwrap();
        let heard = hear(&g, |h| !heard_in(h, "before-the-cover").is_empty());
        let before = table();
        rustix::mount::mount(
            "none",
            &m.ext4,
            "tmpfs",
            rustix::mount::MountFlags::empty(),
            None,
        )
        .unwrap();
        let resolved: Vec<Resolved> = {
            let mut r = g.resolver(&before);
            heard_in(&heard, "before-the-cover")
                .into_iter()
                .map(|f| r.resolve(f))
                .collect()
        };
        rustix::mount::unmount(&m.ext4, rustix::mount::UnmountFlags::empty()).unwrap();
        assert_ne!(resolved, Vec::new());
        for r in resolved {
            assert!(matches!(r, Resolved::Failed(_)), "{r:?}");
        }
    }

    /// A folder in a btrfs subvolume that no mount reaches opens, but has no
    /// path here: it is said unreachable, never given another's path.
    #[test]
    fn a_folder_no_mount_reaches_is_unreachable() {
        let Some(m) = machine() else { return };
        let device = table().holding(&m.btrfs).unwrap().source.clone();
        let top = Place::new(&m.ext4, "top-level");
        let mounted = || {
            rustix::mount::mount(
                device.as_os_str(),
                &top.0,
                "btrfs",
                rustix::mount::MountFlags::empty(),
                Some(c"subvolid=5"),
            )
            .unwrap();
        };
        mounted();
        let hidden = top.join("hidden");
        let made = std::process::Command::new("btrfs")
            .args(["-q", "subvolume", "create"])
            .arg(&hidden)
            .status()
            .unwrap();
        assert!(made.success());
        let g = Group::new(1).unwrap();
        g.mark(&m.btrfs, &table()).unwrap();
        std::fs::write(hidden.join("seen-from-nowhere"), b"x").unwrap();
        let heard = hear(&g, |h| !heard_in(h, "seen-from-nowhere").is_empty());
        let hidden_id = id(&hidden);
        rustix::mount::unmount(&top.0, rustix::mount::UnmountFlags::empty()).unwrap();
        let t = table();
        let mut r = g.resolver(&t);
        for f in heard_in(&heard, "seen-from-nowhere") {
            assert_eq!(r.resolve(f), Resolved::Unreachable(hidden_id));
        }
        mounted();
        let removed = std::process::Command::new("btrfs")
            .args(["-q", "subvolume", "delete"])
            .arg(&hidden)
            .status()
            .unwrap();
        rustix::mount::unmount(&top.0, rustix::mount::UnmountFlags::empty()).unwrap();
        assert!(removed.success());
    }

    /// A filesystem that cannot give file handles refuses the mark, and
    /// says why.
    #[test]
    fn a_filesystem_without_handles_is_refused() {
        let Some(m) = machine() else { return };
        let place = Place::new(&m.ext4, "ramfs");
        rustix::mount::mount(
            "none",
            &place.0,
            "ramfs",
            rustix::mount::MountFlags::empty(),
            None,
        )
        .unwrap();
        let g = Group::new(1).unwrap();
        let refused = g.mark(&place.0, &table());
        rustix::mount::unmount(&place.0, rustix::mount::UnmountFlags::empty()).unwrap();
        assert_eq!(refused, Err(Refused::NoHandles));
    }
}
