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

/// A client of the service, once it answers and its index of `root` is
/// ready, waited for at most a minute.
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

/// A folder of `user`'s, made by them at `secret` with a file in it, and
/// their own file of exclusions, in their profile, which names it and
/// `not_theirs`; their output in `bin`.
fn kept_out_by(user: &User, secret: &Path, not_theirs: &Path, bin: &Path) {
    let config = r"%LOCALAPPDATA%\skwird\config";
    let (code, said) = user.run(
        &format!(
            "mkdir \"{secret}\" && echo.> \"{secret}\\notes-secret.txt\" && mkdir \"{config}\" && \
             (echo {secret}& echo {not_theirs})> \"{config}\\exclude\"",
            secret = secret.display(),
            not_theirs = not_theirs.display(),
        ),
        bin,
        Duration::from_secs(60),
    );
    assert_eq!(code, Some(0), "{said}");
}

/// `net` run with `args`, which waits for the service to stop or start.
fn net(args: &[&str]) {
    let out = Command::new("net").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The service stopped and started again resumes `root` from its
/// journal's position kept (DESIGN 2): its stop saves the position with
/// the index; a file made while it was stopped is answered once its
/// journal is replayed, with no sync and no check, and the root is live.
fn stopped_and_started_it_resumes(s: &Scratch, root: &Path) {
    net(&["stop", skwird::service::NAME]);
    let index = skwird::engine::index_dir(&skwird::dirs::Dirs::system(), root);
    let kept = skwird_index::store::read_mark(&index).and_then(|m| m.position);
    assert!(kept.is_some(), "the stop kept its position:\n{}", log());
    s.write(r"root\open\notes-while-stopped.txt", b"");
    let meanwhile = root.join(r"open\notes-while-stopped.txt");
    net(&["start", skwird::service::NAME]);
    let mut c = ready(root);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !answered(&mut c, &named(root, "notes")).contains(&meanwhile) {
        assert!(Instant::now() < deadline, "never replayed:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(c.coverage(root).unwrap().names, Proof::Live(_)) {
        assert!(Instant::now() < deadline, "never live again:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let logged = log();
    let restart = logged.rfind("serving 2 roots").unwrap();
    let since = &logged[restart..];
    assert!(
        since.contains("resumed from its journal's position kept"),
        "{since}"
    );
    assert!(since.contains("its journal replayed, live"), "{since}");
    assert!(!since.contains("checked in"), "no check:\n{since}");
}

/// Where the position kept no longer holds, the root is checked by its
/// volume's file table (DESIGN 2): its journal deleted while the service
/// is stopped, a file made meanwhile is answered once the restart's check
/// is done, by the table, not resumed, and the root is live.
fn a_journal_made_again_is_checked_by_the_table(s: &Scratch, volume: &Path, root: &Path) {
    net(&["stop", skwird::service::NAME]);
    let letter = volume.to_str().unwrap().trim_end_matches('\\');
    let deleted = Command::new("fsutil")
        .args(["usn", "deletejournal", "/n", letter])
        .output()
        .unwrap();
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stdout)
    );
    s.write(r"root\open\notes-journal-gone.txt", b"");
    let meanwhile = root.join(r"open\notes-journal-gone.txt");
    net(&["start", skwird::service::NAME]);
    let mut c = ready(root);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !answered(&mut c, &named(root, "notes")).contains(&meanwhile)
        || !matches!(c.coverage(root).unwrap().names, Proof::Live(_))
    {
        assert!(Instant::now() < deadline, "never checked:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let logged = log();
    let since = &logged[logged.rfind("serving 2 roots").unwrap()..];
    assert!(!since.contains("resumed from"), "{since}");
    assert!(since.contains("to check it by"), "{since}");
    assert!(since.contains("checked in"), "{since}");
}

/// A volume a root is on, taken away while the service runs and given
/// back: its removal asked of the service, the root lets go of every
/// handle of it, so that the removal goes on, and is not live; given back,
/// the root is checked and followed again, live. The spare volume's
/// virtual disk detached and attached again (`diskpart`).
fn a_volume_taken_away_is_let_go_of(s: &Scratch, root: &Path) {
    let disk = skwird_testkit::ntfs_spare_disk().expect("the spare volume's disk");
    let select = format!("select vdisk file=\"{}\"", disk.display());
    let live = |c: &mut Client| matches!(c.coverage(root).map(|c| c.names), Ok(Proof::Live(_)));
    let mut c = ready(root);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !live(&mut c) {
        assert!(Instant::now() < deadline, "never live:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let (detached, said) =
        skwird_testkit::diskpart(&[select.clone(), "detach vdisk".into()], s.path());
    assert!(detached, "the volume in use?\n{said}\n{}", log());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !log().contains("its volume is being removed, let go of") {
        assert!(Instant::now() < deadline, "never let go of:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!live(&mut c), "not live while it is away");
    let (attached, said) = skwird_testkit::diskpart(&[select, "attach vdisk".into()], s.path());
    assert!(attached, "{said}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !live(&mut c) {
        assert!(Instant::now() < deadline, "never live again:\n{}", log());
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(log().contains("watched again, live"), "{}", log());
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
/// - the user's own file of exclusions, in their profile, kept: a folder
///   of theirs it names never indexed, one it names that is not theirs
///   indexed all the same;
/// - stopped and started again, it resumes from its journal's position
///   kept, with no check ([`stopped_and_started_it_resumes`]); its journal
///   made again meanwhile, it checks by the table
///   ([`a_journal_made_again_is_checked_by_the_table`]);
/// - a volume a root is on taken away and given back, let go of, then
///   live again ([`a_volume_taken_away_is_let_go_of`]);
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
    // A folder of theirs, made by them, and their own file of exclusions,
    // which names it and a folder that is not theirs: read as the service
    // starts.
    skwird_testkit::grant(&s.dir(r"root\theirs"), user.name);
    let secret = root.join(r"theirs\secret");
    kept_out_by(&user, &secret, &root.join("open"), &bin);
    let registered = Registered;
    // A second root, on the spare volume, which is taken away and given
    // back ([`a_volume_taken_away_is_let_go_of`]).
    let spare = skwird_testkit::ntfs_spare().expect("a spare volume beside S:");
    let away = Scratch::under(&spare, "service-away");
    away.write(r"there\notes-away.txt", b"");
    let away_root = skwird_crawl::canonical(away.path()).unwrap();
    let said = run(&[
        "service",
        "add",
        "--root",
        root.to_str().unwrap(),
        "--root",
        away_root.to_str().unwrap(),
    ]);
    assert!(said.contains("registered and started"), "{said}");
    let mut c = ready(&root);
    assert_eq!(c.welcome().scope, Scope::System);
    let both: BTreeSet<PathBuf> = [r"open\notes-open.txt", r"closed\notes-closed.txt"]
        .iter()
        .map(|rel| root.join(rel))
        .collect();
    assert_eq!(answered(&mut c, &named(&root, "notes")), both, "{}", log());
    let kept_out = c.coverage(&secret).unwrap();
    assert_eq!(
        kept_out.names,
        Proof::NotCovered(Why::Excluded),
        "{}",
        log()
    );
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
    stopped_and_started_it_resumes(&s, &root);
    a_journal_made_again_is_checked_by_the_table(&s, &volume, &root);
    a_volume_taken_away_is_let_go_of(&s, &away_root);
    let said = run(&["service", "remove"]);
    assert!(said.contains("stopped and taken back"), "{said}");
    std::mem::forget(registered);
    assert!(Client::connect(&system()).is_err(), "its pipe gone");
}
