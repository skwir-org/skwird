//! The system service on Windows, registered with the service manager
//! and run by it as `LocalSystem` (`docs/PLAN.md`, phase 4, step 7),
//! asked by an administrator and by a user of the test's own: each
//! answered what they could list.
//!
//! In a test binary of its own: while the service runs, the system's pipe
//! answers every client on the machine that names no pipe of its own, and
//! cargo runs one test binary at a time. On a machine of the tests' own
//! alone (`skwird_testkit::throwaway`), on its NTFS volume, never the
//! machine's own drives.

#![cfg(windows)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use skwird_client::{Client, Find};
use skwird_protocol::{Links, Match, NameTerm, Proof, Query, Scope, Why, WirePath};
use skwird_testkit::{Scratch, User};

fn program() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_skwird"))
}

/// The service, registered for the test: taken back when dropped,
/// whatever came of the test.
struct Registered;

impl Drop for Registered {
    fn drop(&mut self) {
        let _ = Command::new(program()).args(["service", "remove"]).output();
    }
}

/// What the program said, run as this test's user with `args`.
fn run(args: &[&str]) -> String {
    let out = Command::new(program())
        .args(args)
        .env("SKWIRD_NO_START", "1")
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    said
}

/// The service's log, in its state folder.
fn log() -> String {
    let data = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    std::fs::read_to_string(data.join(r"skwird\state\engine.log")).unwrap_or_default()
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

/// The service, registered and started by `skwird service add`, as
/// `LocalSystem`:
/// - its pipe taken as the system's by an administrator's client and a
///   user's, its scope the system's;
/// - its root built from its volume's file table, and live through its
///   journal: a file made after is answered with no sync;
/// - an administrator answered a folder the administrators alone may list,
///   a user of the test's own not, though both are answered the folder
///   every user may list;
/// - taken back by `skwird service remove`, its pipe gone.
#[test]
fn the_service_answers_each_user_what_they_could_list() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "service");
    s.write(r"root\open\notes-open.txt", b"");
    s.write(r"root\closed\notes-closed.txt", b"");
    skwird_testkit::administrators_alone(&s.join(r"root\closed"));
    let root = skwird_crawl::canonical(s.join("root")).unwrap();
    let user = User::make("skwird-svc", "Sk-1wird.svc!");
    // The program where the user may run it, and a folder they may write
    // what it says to: the runner's build folder may be the runner's alone.
    let bin = s.dir("bin");
    skwird_testkit::grant(&bin, user.name);
    let theirs = bin.join("skwird.exe");
    std::fs::copy(program(), &theirs).unwrap();
    let registered = Registered;
    let said = run(&["service", "add", "--root", root.to_str().unwrap()]);
    assert!(said.contains("registered and started"), "{said}");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut c = loop {
        let asked = Client::connect(&system())
            .map_err(|e| e.to_string())
            .and_then(|mut c| match c.coverage(&root) {
                Ok(cov) if cov.names != Proof::NotCovered(Why::Unread) => Ok(c),
                Ok(cov) => Err(format!("{:?}", cov.names)),
                Err(e) => Err(e.to_string()),
            });
        match asked {
            Ok(c) => break c,
            Err(e) => assert!(
                Instant::now() < deadline,
                "the service never answered ({e}):\n{}",
                log()
            ),
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(c.welcome().scope, Scope::System);
    let both: BTreeSet<PathBuf> = [r"open\notes-open.txt", r"closed\notes-closed.txt"]
        .iter()
        .map(|rel| root.join(rel))
        .collect();
    assert_eq!(answered(&mut c, &named(&root, "notes")), both, "{}", log());
    let logged = log();
    assert!(
        logged.contains("its volume's file table read"),
        "built from the table:\n{logged}"
    );
    // Live through the journal: a file made now answered with no sync.
    let cov = c.coverage(&root).unwrap();
    assert!(matches!(cov.names, Proof::Live(_)), "{cov:?}\n{logged}");
    s.write(r"root\open\notes-later.txt", b"");
    let later = root.join(r"open\notes-later.txt");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !answered(&mut c, &named(&root, "notes")).contains(&later) {
        assert!(Instant::now() < deadline, "never heard:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(c);
    // A user who is not an administrator, through their own client.
    let wait = Duration::from_secs(60);
    let (code, said) = user.run(
        &format!("set SKWIRD_NO_START=1&& \"{}\" status", theirs.display()),
        &bin,
        wait,
    );
    assert_eq!(code, Some(0), "{said}");
    assert!(said.contains("System scope"), "{said}");
    let (code, said) = user.run(
        &format!(
            "set SKWIRD_NO_START=1&& \"{}\" find notes --in \"{}\"",
            theirs.display(),
            root.display()
        ),
        &bin,
        wait,
    );
    assert_eq!(code, Some(0), "{said}\n{}", log());
    assert!(said.contains("notes-open.txt"), "{said}");
    assert!(said.contains("notes-later.txt"), "{said}");
    assert!(!said.contains("notes-closed.txt"), "{said}");
    let said = run(&["service", "remove"]);
    assert!(said.contains("stopped and taken back"), "{said}");
    std::mem::forget(registered);
    assert!(Client::connect(&system()).is_err(), "its pipe gone");
}
