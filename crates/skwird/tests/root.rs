//! The system service as root, in the test machine (`tools/vm/root.nix`,
//! `docs/PLAN.md`, phase 3, step 4): whole filesystems marked, every
//! change heard, answered live. Each test is skipped anywhere else.
//!
//! The service's folders are the test's own (its service manager's
//! `RUNTIME_DIRECTORY` and the like), and its roots are named: it never
//! covers the machine's own filesystems.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use skwird_client::Client;
use skwird_protocol::{Match, NameTerm, Proof, Query, Scope, WirePath};
use skwird_testkit::{Place, machine};

/// The program: the machine's copy of it, or cargo's.
fn program() -> PathBuf {
    std::env::var_os("SKWIRD_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_skwird")),
        PathBuf::from,
    )
}

/// A system service of `roots`, its folders below `dirs`, stopped when
/// dropped.
struct Service {
    child: Child,
    socket: PathBuf,
}

impl Service {
    fn start(dirs: &Path, roots: &[&Path]) -> Service {
        let mut c = Command::new(program());
        c.args(["serve", "--system", "--forever", "--threads", "2"]);
        for root in roots {
            c.arg("--root").arg(root);
        }
        for (var, sub) in [
            ("RUNTIME_DIRECTORY", "run"),
            ("CACHE_DIRECTORY", "cache"),
            ("STATE_DIRECTORY", "state"),
            ("CONFIGURATION_DIRECTORY", "config"),
        ] {
            c.env(var, dirs.join(sub));
        }
        Service {
            child: c.spawn().expect("cannot start the service"),
            socket: dirs.join("run/socket"),
        }
    }

    /// A client of it, once it answers for `root`.
    fn client(&self, root: &Path) -> Client {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(mut c) = Client::at(&self.socket, "root-test")
                && c.coverage(root).is_ok()
            {
                return c;
            }
            assert!(Instant::now() < deadline, "the service never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn named(root: &Path, text: &str) -> Query {
    Query {
        root: WirePath::of(root),
        name: Some(NameTerm {
            how: Match::Skwir,
            text: text.to_owned(),
            case_sensitive: false,
        }),
        kinds: None,
        size: None,
        mtime: None,
        limit: None,
    }
}

fn answered(c: &mut Client, query: &Query) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    c.query(query, |batch| {
        paths.extend(batch.iter().map(|e| e.path.to_path()));
        true
    })
    .unwrap();
    paths
}

fn wait_for(seconds: u64, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn live(c: &mut Client, root: &Path) -> bool {
    matches!(c.coverage(root).unwrap().names, Proof::Live(_))
}

/// The system service marks the filesystems of its roots, ext4 and a
/// btrfs subvolume, and answers them live: what was there, a change made
/// with no sync, one made just before a sync (the barrier), a folder
/// removed.
#[test]
fn the_service_hears_whole_filesystems_live() {
    let Some(m) = machine() else { return };
    let ext4 = Place::new(&m.ext4, "service");
    let btrfs = Place::new(&m.btrfs.join("a"), "service");
    std::fs::write(ext4.join("there before.txt"), b"x").unwrap();
    std::fs::create_dir_all(btrfs.join("deep/er")).unwrap();
    let dirs = Place::new(&m.ext4, "service-dirs");
    let service = Service::start(&dirs.0, &[&ext4.0, &btrfs.0]);
    let mut c = service.client(&ext4.0);
    assert_eq!(c.welcome().scope, Scope::System);
    for root in [&ext4.0, &btrfs.0] {
        wait_for(20, "live", || live(&mut c, root));
    }
    assert_eq!(
        answered(&mut c, &named(&ext4.0, "there before")),
        [ext4.join("there before.txt")].into()
    );
    std::fs::write(btrfs.join("deep/er/made later.txt"), b"x").unwrap();
    wait_for(5, "a change heard, with no sync", || {
        answered(&mut c, &named(&btrfs.0, "made later"))
            == [btrfs.join("deep/er/made later.txt")].into()
    });
    std::fs::write(ext4.join("just before the sync"), b"x").unwrap();
    let synced = c.sync(&ext4.0).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{:?}", synced.names);
    assert_eq!(
        answered(&mut c, &named(&ext4.0, "just before")),
        [ext4.join("just before the sync")].into()
    );
    std::fs::remove_dir_all(btrfs.join("deep")).unwrap();
    wait_for(5, "a folder removed heard", || {
        answered(&mut c, &named(&btrfs.0, "made later")).is_empty()
    });
    assert!(live(&mut c, &btrfs.0));
}

/// A folder below `place`, made with this owner, group and mode.
fn folder(place: &Place, rel: &str, owner: u32, group: u32, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = place.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::os::unix::fs::chown(&path, Some(owner), Some(group)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// What the service answers a user, as they see it: every entry found
/// below `root`, and the folders said not covered, each prefixed so. Run
/// as that user, by the test binary run again through `setpriv`.
fn as_user(socket: &Path, root: &Path, uid: u32, gid: u32, groups: &[u32]) -> BTreeSet<String> {
    let groups: Vec<String> = groups.iter().map(u32::to_string).collect();
    let mut c = Command::new("setpriv");
    c.args([
        format!("--reuid={uid}"),
        format!("--regid={gid}"),
        if groups.is_empty() {
            "--clear-groups".to_owned()
        } else {
            format!("--groups={}", groups.join(","))
        },
    ])
    .arg(std::env::current_exe().unwrap())
    .args(["--exact", "an_asker", "--nocapture", "--test-threads=1"])
    .env("SKWIRD_TEST_PEER", socket)
    .env("SKWIRD_TEST_ROOT", root);
    let out = c.output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && said.contains("1 passed"),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    said.lines()
        // The harness writes `test an_asker ... ` before the first line, with
        // no newline.
        .filter_map(|l| l.split_once("seen: ").map(|(_, rest)| rest))
        .map(str::to_owned)
        .collect()
}

/// Run by the test below as another user: says each entry found, and each
/// folder said not covered.
#[test]
fn an_asker() {
    let (Some(socket), Some(root)) = (
        std::env::var_os("SKWIRD_TEST_PEER"),
        std::env::var_os("SKWIRD_TEST_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    let mut c = Client::at(Path::new(&socket), "an-asker").unwrap();
    let everything = Query {
        name: None,
        ..named(&root, "")
    };
    let mut seen = Vec::new();
    let done = c
        .query(&everything, |batch| {
            seen.extend(batch.iter().map(|e| e.path.to_path()));
            true
        })
        .unwrap();
    for path in seen {
        let rel = path.strip_prefix(&root).unwrap();
        println!("seen: found {}", rel.display());
    }
    // A folder they may not list, and a path in it that is not there, are
    // answered alike: whether it exists is not said.
    for rel in ["someone", "someone/not there"] {
        let said = match c.coverage(&root.join(rel)) {
            Ok(cov) => match cov.names {
                Proof::NotCovered(why) => format!("{why:?}"),
                _ => "covered".to_owned(),
            },
            Err(_) => "no folder".to_owned(),
        };
        println!("seen: covers {rel}: {said}");
    }
    for hole in done.coverage.holes {
        let rel = hole.path.to_path();
        println!("seen: hole {}", rel.strip_prefix(&root).unwrap().display());
    }
}

/// The system service answers every user what they could list (DESIGN 4):
/// a folder only its owner may read, one its group may, one any may, one
/// that may be passed through but not listed, and one with an ACL, whose
/// names are withheld from all but root, and said not covered.
#[test]
fn each_user_is_answered_what_they_could_list() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.ext4, "askers");
    folder(&place, "", 0, 0, 0o755);
    let own = folder(&place, "someone", 1000, 1000, 0o700);
    std::fs::write(own.join("someone's secret.txt"), b"x").unwrap();
    let open = folder(&place, "shared", 1001, 1001, 0o755);
    std::fs::write(open.join("open to all.txt"), b"x").unwrap();
    let team = folder(&place, "team", 0, 2000, 0o750);
    std::fs::write(team.join("for the team.txt"), b"x").unwrap();
    folder(&place, "passage", 0, 0, 0o711);
    let inner = folder(&place, "passage/inner", 0, 0, 0o755);
    std::fs::write(inner.join("through a passage.txt"), b"x").unwrap();
    let acl = folder(&place, "acl", 0, 0, 0o700);
    std::fs::write(acl.join("granted by an acl.txt"), b"x").unwrap();
    let granted = Command::new("setfacl")
        .args(["-m", "u:1001:rx"])
        .arg(&acl)
        .status()
        .unwrap();
    assert!(granted.success());
    let dirs = Place::new(&m.ext4, "askers-dirs");
    let service = Service::start(&dirs.0, &[&place.0]);
    let mut c = service.client(&place.0);
    wait_for(20, "live", || live(&mut c, &place.0));
    let set =
        |items: &[&str]| -> BTreeSet<String> { items.iter().map(|s| (*s).to_owned()).collect() };
    let everyone = [
        "found someone",
        "found shared",
        "found shared/open to all.txt",
        "found team",
        "found passage",
        "found passage/inner/through a passage.txt",
        "found acl",
        "hole acl",
    ];
    let mut someone = set(&everyone);
    someone.extend(set(&[
        "found someone/someone's secret.txt",
        "found team/for the team.txt",
        "covers someone: covered",
        "covers someone/not there: no folder",
    ]));
    assert_eq!(
        as_user(&service.socket, &place.0, 1000, 1000, &[2000]),
        someone
    );
    let mut other = set(&everyone);
    other.extend(set(&[
        "covers someone: Denied",
        "covers someone/not there: Denied",
    ]));
    assert_eq!(as_user(&service.socket, &place.0, 1001, 1001, &[]), other);
    // Root sees everything, and nothing is withheld from it.
    let all = as_user(&service.socket, &place.0, 0, 0, &[]);
    assert!(all.contains("found acl/granted by an acl.txt"), "{all:?}");
    assert!(all.contains("found passage/inner"), "{all:?}");
    assert!(!all.iter().any(|s| s.starts_with("hole")), "{all:?}");
}
