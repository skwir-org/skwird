//! The system service on macOS, a `LaunchDaemon` run by launchd as root
//! (`docs/PLAN.md`, phase 5, step 7), asked by this test's user and by
//! `nobody`: each answered what they could list.
//!
//! In a test binary of its own: while the service runs, the system's
//! socket answers every client on the machine that names no socket of its
//! own, and cargo runs one test binary at a time. On a machine of the
//! tests' own alone (`skwird_testkit::throwaway`), root by `sudo`, its
//! root a folder of the test's own in `/Users/Shared`, where `nobody` may
//! reach it, never the machine's own folders.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use skwird_client::{Client, Find};
use skwird_protocol::{Links, Match, NameTerm, Proof, Query, Why, WirePath};

fn program() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_skwird"))
}

/// `args` run as root, asking no password (`sudo -n`).
fn sudo(args: &[&OsStr]) -> Output {
    Command::new("sudo").arg("-n").args(args).output().unwrap()
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The service, registered for the test: taken back when dropped,
/// whatever came of the test.
struct Registered;

impl Drop for Registered {
    fn drop(&mut self) {
        let _ = sudo(&[program().as_os_str(), "service".as_ref(), "remove".as_ref()]);
    }
}

/// The service's log, in its folder of logs.
fn log() -> String {
    said(&sudo(&[
        "cat".as_ref(),
        "/Library/Logs/skwird/engine.log".as_ref(),
    ]))
}

/// A folder of the test's own in `/Users/Shared`, which every user may
/// reach, removed when dropped.
struct Shared(PathBuf);

impl Shared {
    fn new(label: &str) -> Shared {
        let dir =
            Path::new("/Users/Shared/skwird-tests").join(format!("{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for d in [dir.parent().unwrap(), &dir] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Shared(dir)
    }

    /// A folder at `rel`, of mode `mode`, with a file `file` in it.
    fn folder(&self, rel: &str, mode: u32, file: &str) -> PathBuf {
        let dir = self.0.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), b"x").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        dir
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn system() -> Find {
    Find {
        socket: Some(skwird_client::system_socket()),
        start: None,
        ..Find::new("a test")
    }
}

fn named(root: &Path, text: &str) -> Query {
    Query {
        root: WirePath::of(root),
        name: Some(NameTerm {
            how: Match::Skwir,
            text: text.into(),
            case_sensitive: false,
        }),
        kinds: None,
        size: None,
        mtime: None,
        limit: None,
        links: Links::Own,
    }
}

fn answered(c: &mut Client, query: &Query) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    c.query(query, |batch| {
        found.extend(batch.iter().map(|e| e.path.to_path()));
        true
    })
    .unwrap();
    found
}

/// A client of the service, once it answers and its index of `root` is
/// read, waited for at most a minute.
fn ready(root: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let asked = Client::connect(&system())
            .map_err(|e| e.to_string())
            .and_then(|mut c| match c.coverage(root) {
                Ok(cov) if cov.names != Proof::NotCovered(Why::Unread) => Ok(c),
                Ok(cov) => Err(format!("{:?}", cov.names)),
                Err(e) => Err(e.to_string()),
            });
        match asked {
            Ok(c) => return c,
            Err(e) => assert!(
                Instant::now() < deadline,
                "the service never answered ({e}):\n{}",
                log()
            ),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// What `nobody` is answered, asking `skwird find` with the program at
/// `bin`, which they may run.
fn as_nobody(bin: &Path, root: &Path, text: &str) -> BTreeSet<PathBuf> {
    let out = sudo(&[
        "-u".as_ref(),
        "nobody".as_ref(),
        "env".as_ref(),
        "SKWIRD_NO_START=1".as_ref(),
        bin.as_os_str(),
        "find".as_ref(),
        text.as_ref(),
        "--in".as_ref(),
        root.as_os_str(),
    ]);
    assert!(out.status.success(), "{}\n{}", said(&out), log());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .collect()
}

/// The service, registered by `sudo skwird service add` with a root of
/// the test's own, runs as root and answers each user what they could
/// list: its owner every name, `nobody` those of folders open to every
/// user alone, as each folder's ACL says first (macOS' extended ones):
/// one that denies them listing hides what its mode would show, one that
/// lets them in shows what its mode would hide. Its index and state are
/// its own, its socket every user's; taken back, it answers no more.
#[test]
fn the_service_answers_each_user_what_they_could_list() {
    if !skwird_testkit::throwaway() {
        eprintln!(
            "skipped: it would register a system service of the machine's (SKWIRD_TEST_MACHINE)"
        );
        return;
    }
    let s = Shared::new("service");
    s.folder("root/open", 0o755, "public-notes.txt");
    s.folder("root/closed", 0o700, "secret-notes.txt");
    let denied = s.folder("root/denied", 0o755, "denied-notes.txt");
    let let_in = s.folder("root/let-in", 0o700, "let-in-notes.txt");
    for (folder, entry) in [
        (&denied, "user:nobody deny list"),
        (&let_in, "user:nobody allow list,search"),
    ] {
        let added = Command::new("chmod")
            .args(["+a", entry])
            .arg(folder)
            .output()
            .unwrap();
        assert!(added.status.success(), "{}", said(&added));
    }
    let root = skwird_crawl::canonical(s.0.join("root")).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The program `nobody` runs, where they may run it.
    let bin = s.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let theirs = bin.join("skwird");
    std::fs::copy(program(), &theirs).unwrap();
    std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();

    let added = sudo(&[
        program().as_os_str(),
        "service".as_ref(),
        "add".as_ref(),
        "--root".as_ref(),
        root.as_os_str(),
    ]);
    let _registered = Registered;
    assert!(added.status.success(), "{}", said(&added));
    let printed = said(&sudo(&[
        "launchctl".as_ref(),
        "print".as_ref(),
        "system/org.skwir.skwird-system".as_ref(),
    ]));
    assert!(printed.contains("state = running"), "{printed}");

    let mut c = ready(&root);
    let open = root.join("open/public-notes.txt");
    let closed = root.join("closed/secret-notes.txt");
    let denied = root.join("denied/denied-notes.txt");
    let let_in = root.join("let-in/let-in-notes.txt");
    let mine = answered(&mut c, &named(&root, "notes"));
    for theirs in [&open, &closed, &denied, &let_in] {
        assert!(mine.contains(theirs), "{mine:?}");
    }
    let nobodys = as_nobody(&theirs, &root, "notes");
    assert!(nobodys.contains(&open), "{nobodys:?}\n{}", log());
    assert!(!nobodys.contains(&closed), "{nobodys:?}");
    assert!(!nobodys.contains(&denied), "denied by its ACL: {nobodys:?}");
    assert!(
        nobodys.contains(&let_in),
        "let in by its ACL: {nobodys:?}\n{}",
        log()
    );

    // Its folders: its index and state its own, its socket every user's.
    for own in [
        "/Library/Caches/skwird",
        "/Library/Application Support/skwird/state",
        "/Library/Application Support/skwird/config",
    ] {
        let meta = std::fs::metadata(own).unwrap();
        assert_eq!((meta.uid(), meta.mode() & 0o777), (0, 0o700), "{own}");
    }
    let socket = std::fs::metadata("/var/run/skwird/socket").unwrap();
    assert_eq!(socket.mode() & 0o777, 0o666);

    drop(c);
    let removed = sudo(&[program().as_os_str(), "service".as_ref(), "remove".as_ref()]);
    assert!(removed.status.success(), "{}", said(&removed));
    assert!(!Path::new("/Library/LaunchDaemons/org.skwir.skwird-system.plist").exists());
    assert!(Client::connect(&system()).is_err(), "its socket gone");
}
