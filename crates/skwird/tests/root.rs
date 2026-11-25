//! The system service as root, in the test machine (`tools/vm/root.nix`,
//! `docs/PLAN.md`, phase 3, step 4): whole filesystems marked, every
//! change heard, answered live. Each test is skipped anywhere else.
//!
//! The service's folders are the test's own (its service manager's
//! `RUNTIME_DIRECTORY` and the like), and its roots are named: it never
//! covers the machine's own filesystems.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fmt::Write;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use skwird_client::Client;
use skwird_protocol::{Links, Match, NameTerm, Proof, Query, Scope, WirePath};
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
        links: Links::Own,
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
    as_user_with(socket, root, uid, gid, groups, None)
}

/// [`as_user`], the user keeping `exclude` out of the index first.
fn as_user_with(
    socket: &Path,
    root: &Path,
    uid: u32,
    gid: u32,
    groups: &[u32],
    exclude: Option<&Path>,
) -> BTreeSet<String> {
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
    if let Some(folder) = exclude {
        c.env("SKWIRD_TEST_EXCLUDE", folder);
    }
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
    // A folder this user keeps out of the index, if the test asks.
    if let Some(folder) = std::env::var_os("SKWIRD_TEST_EXCLUDE") {
        c.exclude(&[Path::new(&folder)], &[]).unwrap();
    }
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
/// that may be passed through but not listed, and one with an ACL that
/// names one user: answered to them, as the kernel would let them list it.
#[test]
fn each_user_is_answered_what_they_could_list() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.ext4, "askers");
    folder(&place, "", 0, 0, 0o755);
    let own = folder(&place, "someone", 1000, 1000, 0o700);
    std::fs::write(own.join("someone's secret.txt"), b"x").unwrap();
    folder(&place, "someone/kept private", 1000, 1000, 0o700);
    owned_file(&place, "someone/kept private/.private", 1000, 0o600, "");
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
    ];
    let mut someone = set(&everyone);
    someone.extend(set(&[
        "found someone/someone's secret.txt",
        "found team/for the team.txt",
        "covers someone: covered",
        // Their own private folder is a hole they are told of; the other
        // user, who may not list their folder, is not.
        "hole someone/kept private",
        "found someone/kept private",
        "covers someone/not there: no folder",
    ]));
    assert_eq!(
        as_user(&service.socket, &place.0, 1000, 1000, &[2000]),
        someone
    );
    let mut other = set(&everyone);
    other.extend(set(&[
        "found acl/granted by an acl.txt",
        "covers someone: Denied",
        "covers someone/not there: Denied",
    ]));
    assert_eq!(as_user(&service.socket, &place.0, 1001, 1001, &[]), other);
    // Root sees everything, and nothing is withheld from it.
    let all = as_user(&service.socket, &place.0, 0, 0, &[]);
    assert!(all.contains("found acl/granted by an acl.txt"), "{all:?}");
    assert!(all.contains("found passage/inner"), "{all:?}");
    // Root is told of every hole: the private folder alone.
    let holes: Vec<&String> = all.iter().filter(|s| s.starts_with("hole")).collect();
    assert_eq!(holes, ["hole someone/kept private"], "{all:?}");
    // The ACL changed while it runs: heard, read again, answered at once.
    let granted = Command::new("setfacl")
        .args(["-m", "u:1000:rx"])
        .arg(&acl)
        .status()
        .unwrap();
    assert!(granted.success());
    wait_for(5, "an ACL granted heard", || {
        as_user(&service.socket, &place.0, 1000, 1000, &[2000])
            .contains("found acl/granted by an acl.txt")
    });
}

/// Two users the machine's user database names for the test, their homes
/// in its place: added to `/etc/passwd`, which is put back as it was when
/// dropped.
struct Users {
    before: String,
}

impl Users {
    fn add(entries: &[(u32, &Path)]) -> Users {
        let before = std::fs::read_to_string("/etc/passwd").unwrap();
        let mut text = before.clone();
        for (uid, home) in entries {
            let _ = writeln!(text, "user{uid}:x:{uid}:{uid}::{}:/bin/sh", home.display());
        }
        std::fs::write("/etc/passwd", text).unwrap();
        Users { before }
    }
}

impl Drop for Users {
    fn drop(&mut self) {
        let _ = std::fs::write("/etc/passwd", &self.before);
    }
}

/// A file below `place`, made with this owner and mode.
fn owned_file(place: &Place, rel: &str, owner: u32, mode: u32, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = place.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    std::os::unix::fs::chown(&path, Some(owner), Some(owner)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Each user's own exclusions hold at the system service, for folders they
/// own alone (DESIGN 5): their file and what their engine's clients
/// registered, read from their home before any client of theirs connects;
/// a file others may write is not theirs; what they register with the
/// service, at once; another's folder, never.
#[test]
fn each_users_own_exclusions_hold_for_their_own_folders() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.ext4, "homes");
    let one = folder(&place, "one", 1000, 1000, 0o755);
    let two = folder(&place, "two", 1001, 1001, 0o755);
    for (rel, owner) in [
        ("one/projects", 1000),
        ("one/hidden by their file", 1000),
        ("one/hidden by their engine", 1000),
        ("one/not theirs", 1001),
        ("two/kept", 1001),
        ("one/a folder of theirs", 1000),
    ] {
        folder(&place, rel, owner, owner, 0o755);
        owned_file(&place, &format!("{rel}/inside.txt"), owner, 0o644, "x");
    }
    for dir in [
        "one/.config",
        "one/.config/skwird",
        "one/.local",
        "one/.local/state",
        "one/.local/state/skwird",
        "two/.config",
        "two/.config/skwird",
    ] {
        let owner = if dir.starts_with("one") { 1000 } else { 1001 };
        folder(&place, dir, owner, owner, 0o755);
    }
    owned_file(
        &place,
        "one/.config/skwird/exclude",
        1000,
        // Group-writable, as umask 002 leaves it: their group is theirs alone.
        0o664,
        "# theirs\n~/hidden by their file\n~/not theirs\n",
    );
    owned_file(
        &place,
        "one/.local/state/skwird/registered",
        1000,
        0o600,
        &format!(
            "[{{\"client\":\"skwir\",\"path\":\"{}\"}}]",
            one.join("hidden by their engine").display()
        ),
    );
    // Their default, a link to the other's folder: resolved as the
    // service, it would take that folder out of everyone's answers.
    std::os::unix::fs::symlink(one.join("a folder of theirs"), two.join(".gnupg")).unwrap();
    // Others may write it: not theirs to say.
    owned_file(
        &place,
        "two/.config/skwird/exclude",
        1001,
        0o666,
        "~/kept\n",
    );
    let _users = Users::add(&[(1000, &one), (1001, &two)]);
    let dirs = Place::new(&m.ext4, "homes-dirs");
    let service = Service::start(&dirs.0, &[&place.0]);
    let mut c = service.client(&place.0);
    wait_for(20, "live", || live(&mut c, &place.0));
    let found = |c: &mut Client| -> BTreeSet<PathBuf> {
        answered(c, &named(&place.0, "inside"))
            .into_iter()
            .map(|p| p.strip_prefix(&place.0).unwrap().to_path_buf())
            .collect()
    };
    let paths = |rels: &[&str]| -> BTreeSet<PathBuf> {
        rels.iter()
            .map(|r| PathBuf::from(r).join("inside.txt"))
            .collect()
    };
    assert_eq!(
        found(&mut c),
        paths(&[
            "one/projects",
            "one/not theirs",
            "two/kept",
            "one/a folder of theirs"
        ])
    );
    // Registered with the service: theirs at once; another's, never.
    as_user_with(
        &service.socket,
        &place.0,
        1000,
        1000,
        &[],
        Some(&one.join("projects")),
    );
    // A user asks the service for such work once a second at most.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    as_user_with(
        &service.socket,
        &place.0,
        1000,
        1000,
        &[],
        Some(&two.join("kept")),
    );
    assert_eq!(
        found(&mut c),
        paths(&["one/not theirs", "two/kept", "one/a folder of theirs"])
    );
}

/// A query of the system service gives at most 100,000 results, and says
/// there were more: any local user may ask it.
#[test]
fn a_query_gives_at_most_so_many_results() {
    let Some(m) = machine() else { return };
    // On btrfs, whose inodes are made as needed: the ext4 holds 65,536.
    let place = Place::new(&m.btrfs, "many");
    for i in 0..100_050 {
        std::fs::File::create(place.join(&format!("f{i}"))).unwrap();
    }
    let dirs = Place::new(&m.ext4, "many-dirs");
    let service = Service::start(&dirs.0, &[&place.0]);
    let mut c = service.client(&place.0);
    wait_for(60, "live", || live(&mut c, &place.0));
    let mut count = 0;
    let done = c
        .query(&named(&place.0, "f"), |batch| {
            count += batch.len();
            true
        })
        .unwrap();
    assert_eq!(count, 100_000);
    assert!(done.truncated);
}

/// Run by the test below as another user: says what the journal tells
/// them since a token.
#[test]
fn a_journal_reader() {
    let (Some(socket), Some(root), Some(since)) = (
        std::env::var_os("SKWIRD_TEST_PEER"),
        std::env::var_os("SKWIRD_TEST_ROOT"),
        std::env::var("SKWIRD_TEST_SINCE").ok(),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    let mut c = Client::at(Path::new(&socket), "a-journal-reader").unwrap();
    let changes = c.changes(&root, &since).unwrap().expect("not too old");
    for e in changes.events {
        let (what, path) = match e {
            skwird_protocol::Event::Added(e) => ("added", e.path.to_path()),
            skwird_protocol::Event::Removed(e) => ("removed", e.path.to_path()),
            skwird_protocol::Event::Changed(e) => ("changed", e.path.to_path()),
            skwird_protocol::Event::NotCovered { path, .. } => ("not covered", path.to_path()),
        };
        println!(
            "seen: {what} {}",
            path.strip_prefix(&root).unwrap().display()
        );
    }
}

/// The journal of the system service tells each user what changed in
/// folders they may list, and nothing of the others'.
#[test]
fn the_journal_tells_each_user_what_they_could_list() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.ext4, "journal");
    folder(&place, "", 0, 0, 0o755);
    folder(&place, "theirs", 1000, 1000, 0o700);
    folder(&place, "open", 0, 0, 0o755);
    let dirs = Place::new(&m.ext4, "journal-dirs");
    let service = Service::start(&dirs.0, &[&place.0]);
    let mut c = service.client(&place.0);
    wait_for(20, "live", || live(&mut c, &place.0));
    let start = c.changes(&place.0, "").unwrap().expect("a token from now");
    owned_file(&place, "theirs/made in theirs.txt", 1000, 0o600, "x");
    owned_file(&place, "open/made in the open.txt", 0, 0o644, "x");
    c.sync(&place.0).unwrap();
    let read = |uid: u32| -> BTreeSet<String> {
        let out = Command::new("setpriv")
            .args([
                format!("--reuid={uid}"),
                format!("--regid={uid}"),
                "--clear-groups".into(),
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_journal_reader",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SKWIRD_TEST_PEER", &service.socket)
            .env("SKWIRD_TEST_ROOT", &place.0)
            .env("SKWIRD_TEST_SINCE", &start.token)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && said.contains("1 passed"), "{said}");
        said.lines()
            .filter_map(|l| l.split_once("seen: ").map(|(_, rest)| rest.to_owned()))
            .filter(|l| l.starts_with("added"))
            .collect()
    };
    let both: BTreeSet<String> = [
        "added theirs/made in theirs.txt",
        "added open/made in the open.txt",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(read(1000), both);
    assert_eq!(
        read(1001),
        ["added open/made in the open.txt".to_owned()].into()
    );
    assert_eq!(read(0), both);
}

/// A query of a folder above another root answers from both, and its
/// coverage holds the other at its place: no mount hole there, live.
#[test]
fn a_query_above_another_root_answers_from_both() {
    let Some(m) = machine() else { return };
    let outer = Place::new(&m.ext4, "outer");
    std::fs::write(outer.join("in the outer.txt"), b"x").unwrap();
    std::fs::create_dir_all(outer.join("inner")).unwrap();
    let source = Place::new(&m.btrfs.join("a"), "inner-source");
    std::fs::write(source.join("in the inner.txt"), b"x").unwrap();
    let mount = |args: &[&std::ffi::OsStr]| {
        assert!(Command::new("mount").args(args).status().unwrap().success());
    };
    mount(&[
        "--bind".as_ref(),
        source.0.as_os_str(),
        outer.join("inner").as_os_str(),
    ]);
    let dirs = Place::new(&m.ext4, "outer-dirs");
    let service = Service::start(&dirs.0, &[&outer.0, &outer.join("inner")]);
    let mut c = service.client(&outer.0);
    wait_for(20, "live", || {
        live(&mut c, &outer.0) && live(&mut c, &outer.join("inner"))
    });
    let found = answered(&mut c, &named(&outer.0, "in the"));
    let coverage = c.coverage(&outer.0).unwrap();
    drop(c);
    drop(service);
    assert!(
        Command::new("umount")
            .arg(outer.join("inner"))
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        found,
        [
            outer.join("in the outer.txt"),
            outer.join("inner/in the inner.txt")
        ]
        .into()
    );
    assert!(
        matches!(coverage.names, Proof::Live(_)),
        "{:?}",
        coverage.names
    );
    assert_eq!(coverage.holes, Vec::new());
}

/// A folder above a root, closed while the service runs, closes what the
/// root holds for those it no longer lets through: judged as the root
/// holding it says now, not as it was at the start.
#[test]
fn a_folder_closed_above_a_root_closes_the_root() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.ext4, "above");
    folder(&place, "", 0, 0, 0o755);
    let theirs = folder(&place, "theirs", 1000, 1000, 0o755);
    folder(&place, "theirs/data", 1000, 1000, 0o755);
    let source = Place::new(&m.btrfs.join("a"), "data-source");
    std::fs::write(source.join("on its own disk.txt"), b"x").unwrap();
    let data = theirs.join("data");
    assert!(
        Command::new("mount")
            .args(["--bind".as_ref(), source.0.as_os_str(), data.as_os_str()])
            .status()
            .unwrap()
            .success()
    );
    let dirs = Place::new(&m.ext4, "above-dirs");
    let service = Service::start(&dirs.0, &[&place.0, &data]);
    let mut c = service.client(&place.0);
    wait_for(20, "live", || live(&mut c, &place.0) && live(&mut c, &data));
    // Asked at the root below itself: only the folders above judge it.
    let found = "found on its own disk.txt";
    let before = as_user(&service.socket, &data, 1001, 1001, &[]);

    std::fs::set_permissions(&theirs, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    wait_for(5, "the folder closed heard", || {
        !as_user(&service.socket, &data, 1001, 1001, &[]).contains(found)
    });
    let owner = as_user(&service.socket, &data, 1000, 1000, &[]);
    drop(c);
    drop(service);
    assert!(
        Command::new("umount")
            .arg(&data)
            .status()
            .unwrap()
            .success()
    );
    assert!(before.contains(found), "{before:?}");
    assert!(owner.contains(found), "its owner still: {owner:?}");
}

/// A folder found once is found again once it moves (DESIGN 8). Here its
/// old parent is large and was just applied, so its share holds it: the
/// folder is read at its new place first, while the index still holds it
/// at the old one, with the same identity. What is heard in it then is
/// applied at its new place, never at the old one, where it would be lost.
/// (Built optimised, the old parent's hold may end before the write: the
/// test then proves less.)
#[test]
fn a_folder_found_then_moved_is_heard_at_its_new_place() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.btrfs, "moved");
    std::fs::create_dir_all(place.join("old/inner")).unwrap();
    std::fs::create_dir_all(place.join("new")).unwrap();
    for i in 0..59_000 {
        std::fs::File::create(place.join(&format!("old/entry-{i:06}"))).unwrap();
    }
    let dirs = Place::new(&m.ext4, "moved-dirs");
    let service = Service::start(&dirs.0, &[&place.0]);
    let mut c = service.client(&place.0);
    wait_for(60, "live", || live(&mut c, &place.0));
    let at = |c: &mut Client, name: &str| answered(c, &named(&place.0, name));
    // Heard, the folder is found, and kept as found.
    std::fs::write(place.join("old/inner/one.txt"), b"x").unwrap();
    wait_for(5, "a file in the folder", || {
        !at(&mut c, "one.txt").is_empty()
    });
    // Its large parent applied: held now for its share. Reading its 59,000
    // entries is charged to the writer's account (DESIGN 8), as the waits
    // below are: 5 s was too short once on a loaded machine (2026-11-23).
    std::fs::write(place.join("old/busy.txt"), b"x").unwrap();
    wait_for(15, "a file in its parent", || {
        !at(&mut c, "busy.txt").is_empty()
    });
    std::fs::rename(place.join("old/inner"), place.join("new/inner")).unwrap();
    // The parent's read is charged to the writer's account (DESIGN 8):
    // unoptimised, it may hold what follows for the account's window.
    wait_for(15, "the folder at its new place", || {
        at(&mut c, "one.txt").contains(&place.join("new/inner/one.txt"))
    });
    std::fs::write(place.join("new/inner/two.txt"), b"x").unwrap();
    wait_for(15, "a file made at its new place", || {
        at(&mut c, "two.txt") == [place.join("new/inner/two.txt")].into()
    });
    let synced = c.sync(&place.0).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{:?}", synced.names);
    assert_eq!(
        at(&mut c, ".txt"),
        [
            place.join("new/inner/one.txt"),
            place.join("new/inner/two.txt"),
            place.join("old/busy.txt"),
        ]
        .into()
    );
}

/// A folder heard in no index is kept as such only while no index changes
/// its shape (DESIGN 8): moved into a root, it is crawled at its new
/// place, and what is heard in it after is applied there, never taken as
/// in no index still. Its first events, outside every root, are found in
/// none; those after, before the move, are taken so.
#[test]
fn a_folder_heard_in_no_index_then_moved_into_one_is_heard_there() {
    let Some(m) = machine() else { return };
    let place = Place::new(&m.btrfs, "unfound");
    let root = place.join("indexed");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(place.join("outside/inner")).unwrap();
    let dirs = Place::new(&m.ext4, "unfound-dirs");
    let service = Service::start(&dirs.0, &[&root]);
    let mut c = service.client(&root);
    wait_for(60, "live", || live(&mut c, &root));
    let at = |c: &mut Client, name: &str| answered(c, &named(&root, name));
    // Heard outside every root, then a file in the root: the writer takes
    // events in order, so once the file is answered, the folder outside
    // was found in no index, and kept so (a file made changes no shape).
    for (outside, probe) in [("one.txt", "probe-1.txt"), ("two.txt", "probe-2.txt")] {
        std::fs::write(place.join("outside/inner").join(outside), b"x").unwrap();
        std::fs::write(root.join(probe), b"x").unwrap();
        wait_for(15, "a file in the root", || !at(&mut c, probe).is_empty());
    }
    assert!(at(&mut c, "one.txt").is_empty(), "nothing outside the root");
    std::fs::rename(place.join("outside/inner"), root.join("inner")).unwrap();
    wait_for(15, "the folder in the root", || {
        at(&mut c, "one.txt") == [root.join("inner/one.txt")].into()
    });
    std::fs::write(root.join("inner/three.txt"), b"x").unwrap();
    wait_for(15, "a file made in it", || {
        at(&mut c, "three.txt") == [root.join("inner/three.txt")].into()
    });
    let synced = c.sync(&root).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{:?}", synced.names);
    assert_eq!(
        at(&mut c, ".txt"),
        [
            root.join("inner/one.txt"),
            root.join("inner/three.txt"),
            root.join("inner/two.txt"),
            root.join("probe-1.txt"),
            root.join("probe-2.txt"),
        ]
        .into()
    );
}

/// A root on a slow disk delays itself alone (AGENTS.md, "Blocking"): each
/// root is opened on a thread of its own, so another is live, its changes
/// answered and its syncs served, while the slow one is still crawled. A
/// change made in the slow one meanwhile is held, and applied once it is
/// open; a sync of it waits for it.
#[test]
fn a_root_slow_to_open_delays_itself_alone() {
    let Some(m) = machine() else { return };
    let slow = Place::new(&m.slow, "slow-root");
    for a in 0..200 {
        let dir = slow.join(&format!("d{a:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..10 {
            std::fs::File::create(dir.join(format!("entry-{i}"))).unwrap();
        }
    }
    let quick = Place::new(&m.ext4, "quick-root");
    std::fs::write(quick.join("quick.txt"), b"x").unwrap();
    m.cold();
    let dirs = Place::new(&m.ext4, "slow-dirs");
    let service = Service::start(&dirs.0, &[&slow.0, &quick.0]);
    let mut c = service.client(&quick.0);
    wait_for(20, "the quick root live", || live(&mut c, &quick.0));
    assert!(!live(&mut c, &slow.0), "the slow root still opening");
    std::fs::write(quick.join("made later.txt"), b"x").unwrap();
    let synced = c.sync(&quick.0).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{:?}", synced.names);
    assert_eq!(
        answered(&mut c, &named(&quick.0, "made later")),
        [quick.join("made later.txt")].into()
    );
    // Made while the slow root is crawled: some in folders it read already.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!live(&mut c, &slow.0), "the slow root still opening");
    let in_each = |name: &str| -> BTreeSet<PathBuf> {
        (0..200)
            .step_by(10)
            .map(|a| slow.join(&format!("d{a:03}/{name}")))
            .collect()
    };
    let made = in_each("made while opening.txt");
    for path in &made {
        std::fs::write(path, b"x").unwrap();
    }
    // Files written to, twice, in batches apart: what the folder's change
    // time does not say, which the check as it opens does not see, so that
    // the events held alone tell it. The second is held by what was kept
    // of the first, its folders known to be in a root being opened.
    let (once, twice) = (in_each("entry-1"), in_each("entry-2"));
    for path in &once {
        std::fs::write(path, b"abc").unwrap();
    }
    std::thread::sleep(Duration::from_millis(200));
    for path in &twice {
        std::fs::write(path, b"abcde").unwrap();
    }
    assert!(!live(&mut c, &slow.0), "the slow root still opening");
    let synced = c.sync(&slow.0).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{:?}", synced.names);
    assert_eq!(
        answered(&mut c, &named(&slow.0, "made while opening")),
        made
    );
    let sized = |c: &mut Client, name: &str, size: u64| {
        let query = Query {
            size: Some([size, size]),
            ..named(&slow.0, name)
        };
        answered(c, &query)
    };
    assert_eq!(sized(&mut c, "entry-1", 3), once, "written to once");
    assert_eq!(
        sized(&mut c, "entry-2", 5),
        twice,
        "written to in a later batch"
    );
}

/// A user's engine, as root in the machine, of `home`, its folders below
/// `dirs`: killed when dropped.
struct UserEngine {
    child: Child,
    socket: PathBuf,
}

impl UserEngine {
    fn start(dirs: &Path, home: &Path) -> UserEngine {
        let mut c = Command::new(program());
        c.args(["serve", "--forever", "--threads", "2", "--root"])
            .arg(home)
            .env("HOME", home)
            .env("SKWIRD_NO_START", "1");
        for (var, sub) in [
            ("XDG_RUNTIME_DIR", "run"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CONFIG_HOME", "config"),
        ] {
            std::fs::create_dir_all(dirs.join(sub)).unwrap();
            c.env(var, dirs.join(sub));
        }
        UserEngine {
            child: c.spawn().expect("cannot start the engine"),
            socket: dirs.join("run/skwird/socket"),
        }
    }

    fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(c) = Client::at(&self.socket, "a test") {
                return c;
            }
            assert!(Instant::now() < deadline, "the engine never answered");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for UserEngine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(program: &str, args: &[&std::ffi::OsStr]) {
    let done = Command::new(program).args(args).status().unwrap();
    assert!(done.success(), "{program} {args:?}");
}

/// A removable volume a client keeps (DESIGN, Protocol, "Removable
/// volumes"): the machine's USB stick, seen by its FAT serial with its
/// label, not indexed until kept. Kept, then released by its mount point
/// before its first build, as skwir's eject does: answered once nothing of
/// it is held, and unmounted at once. Mounted again, a root at its mount
/// point, its names answered; released by its identity, unmounted; mounted
/// again, a root again, its index read back; forgotten, its index
/// removed.
#[test]
fn a_kept_stick_is_a_root_while_it_is_mounted() {
    let Some(m) = machine() else { return };
    let home = Place::new(&m.ext4, "volumes-home");
    std::fs::create_dir_all(&home.0).unwrap();
    let dirs = Place::new(&m.ext4, "volumes-dirs");
    let at = Place::new(&m.ext4, "volumes-stick");
    std::fs::create_dir_all(&at.0).unwrap();
    let device = m.stick.as_os_str();
    run("mount", &[device, at.0.as_os_str()]);
    std::fs::create_dir_all(at.join("photos")).unwrap();
    std::fs::write(at.join("photos/on the stick.jpg"), b"x").unwrap();
    let engine = UserEngine::start(&dirs.0, &home.0);
    let mut c = engine.client();
    assert!(
        c.welcome()
            .features
            .iter()
            .any(|f| f == skwird_protocol::VOLUMES)
    );
    // Seen, not kept: no root, nothing answered there.
    let v = c.volumes(&[], &[], &[]).unwrap();
    let seen = v
        .seen
        .iter()
        .find(|s| s.at == WirePath::of(&at.0))
        .unwrap_or_else(|| panic!("the stick is not seen: {v:?}"));
    assert_eq!(seen.filesystem, "vfat");
    assert_eq!(seen.label.as_deref(), Some("TEST STICK"));
    assert!(!seen.kept);
    let id = seen.id.clone().expect("a FAT serial");
    assert!(id.len() == 9 && id.as_bytes()[4] == b'-', "{id}");
    assert!(answered(&mut c, &named(&at.0, "on the stick")).is_empty());
    // Kept: a root at its mount point, read a few seconds after; released
    // by where it is mounted before then, and unmounted as soon as that is
    // answered.
    let v = c.volumes(&[&id], &[], &[]).unwrap();
    assert_eq!(v.kept.len(), 1, "{v:?}");
    assert_eq!(v.kept[0].standing, skwird_protocol::Standing::Indexed);
    assert_eq!(v.kept[0].at, Some(WirePath::of(&at.0)));
    let v = c.release_at(&[&at.0]).unwrap();
    assert_eq!(v.kept[0].standing, skwird_protocol::Standing::Released);
    run("umount", &[at.0.as_os_str()]);
    wait_for(10, "the stick away", || {
        c.volumes(&[], &[], &[])
            .is_ok_and(|v| v.kept[0].standing == skwird_protocol::Standing::Away)
    });
    run("mount", &[device, at.0.as_os_str()]);
    let photo = at.join("photos/on the stick.jpg");
    wait_for(30, "the kept stick indexed", || {
        answered(&mut c, &named(&at.0, "on the stick")) == [photo.clone()].into()
    });
    assert!(
        matches!(
            c.coverage(&at.0).unwrap().names,
            Proof::Checked(_) | Proof::Live(_)
        ),
        "{:?}",
        c.coverage(&at.0)
    );
    // Released before an eject: a root no more, and the unmount goes.
    let v = c.volumes(&[], &[], &[&id]).unwrap();
    assert_eq!(v.kept[0].standing, skwird_protocol::Standing::Released);
    assert!(answered(&mut c, &named(&at.0, "on the stick")).is_empty());
    run("umount", &[at.0.as_os_str()]);
    wait_for(10, "the stick away", || {
        c.volumes(&[], &[], &[])
            .is_ok_and(|v| v.kept[0].standing == skwird_protocol::Standing::Away)
    });
    // Mounted again: a root again, its index read back.
    run("mount", &[device, at.0.as_os_str()]);
    wait_for(30, "the stick indexed again", || {
        answered(&mut c, &named(&at.0, "on the stick")) == [photo.clone()].into()
    });
    let indexes = || {
        std::fs::read_dir(dirs.join("cache/skwird"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("volume-"))
            .count()
    };
    assert_eq!(indexes(), 1);
    // Forgotten: a root no more, its index removed.
    let v = c.volumes(&[], &[&id], &[]).unwrap();
    assert!(v.kept.is_empty(), "{v:?}");
    assert!(answered(&mut c, &named(&at.0, "on the stick")).is_empty());
    assert_eq!(indexes(), 0);
    drop(c);
    drop(engine);
    run("umount", &[at.0.as_os_str()]);
}

/// A btrfs on a USB stick, as the user's drives hold it: its mounts have
/// numbers of their own (major 0), never their device's, and it is seen
/// by its source's device, its btrfs UUID its identity; kept, its names
/// are answered.
#[test]
fn a_btrfs_stick_is_seen_by_its_device() {
    let Some(m) = machine() else { return };
    let home = Place::new(&m.ext4, "btrfs-home");
    std::fs::create_dir_all(&home.0).unwrap();
    let dirs = Place::new(&m.ext4, "btrfs-dirs");
    let at = Place::new(&m.ext4, "btrfs-stick");
    std::fs::create_dir_all(&at.0).unwrap();
    run("mount", &[m.stick_btrfs.as_os_str(), at.0.as_os_str()]);
    std::fs::write(at.join("on the btrfs stick.txt"), b"x").unwrap();
    let uuid = Command::new("blkid")
        .args(["-s", "UUID", "-o", "value"])
        .arg(&m.stick_btrfs)
        .output()
        .unwrap();
    let uuid = String::from_utf8(uuid.stdout).unwrap().trim().to_owned();
    assert_eq!(uuid.len(), 36, "{uuid}");
    let engine = UserEngine::start(&dirs.0, &home.0);
    let mut c = engine.client();
    let v = c.volumes(&[], &[], &[]).unwrap();
    let seen = v
        .seen
        .iter()
        .find(|s| s.at == WirePath::of(&at.0))
        .unwrap_or_else(|| panic!("the btrfs stick is not seen: {v:?}"));
    assert_eq!(seen.filesystem, "btrfs");
    assert_eq!(seen.label.as_deref(), Some("TESTBTRFS"));
    assert_eq!(seen.id.as_deref(), Some(uuid.as_str()));
    c.volumes(&[&uuid], &[], &[]).unwrap();
    let file = at.join("on the btrfs stick.txt");
    wait_for(30, "the btrfs stick indexed", || {
        answered(&mut c, &named(&at.0, "on the btrfs")) == [file.clone()].into()
    });
    let v = c.release_at(&[&at.0]).unwrap();
    assert_eq!(v.kept[0].standing, skwird_protocol::Standing::Released);
    drop(c);
    drop(engine);
    run("umount", &[at.0.as_os_str()]);
}
