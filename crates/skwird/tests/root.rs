//! The system service as root, in the test machine (`tools/vm/root.nix`,
//! `docs/PLAN.md`, phase 3, step 4): whole filesystems marked, every
//! change heard, answered live. Each test is skipped anywhere else.
//!
//! The service's folders are the test's own (its service manager's
//! `RUNTIME_DIRECTORY` and the like), and its roots are named: it never
//! covers the machine's own filesystems.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::os::unix::process::CommandExt;
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

/// Until its answers are filtered for each asker (step 5), the system
/// service answers root alone: another user is refused (rule 2).
#[test]
fn the_service_answers_root_alone_for_now() {
    let Some(m) = machine() else { return };
    let root = Place::new(&m.ext4, "alone");
    let dirs = Place::new(&m.ext4, "alone-dirs");
    let service = Service::start(&dirs.0, &[&root.0]);
    service.client(&root.0);
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "as_another_user",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SKWIRD_TEST_PEER", &service.socket)
        .uid(65534)
        .gid(65534)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && said.contains("1 passed"),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run by the test above, as another user: the service does not answer.
#[test]
fn as_another_user() {
    let Some(socket) = std::env::var_os("SKWIRD_TEST_PEER") else {
        return;
    };
    assert!(Client::at(Path::new(&socket), "another").is_err());
}
