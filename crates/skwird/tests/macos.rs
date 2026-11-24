//! A user's engine on macOS, run as its program and asked through the
//! client (`docs/PLAN.md`, phase 5, steps 2 and 3), on GitHub's macOS
//! runners: every root heard whole by one `FSEvents` stream.
//!
//! Every process a test runs is given `HOME` and the XDG folders of a
//! scratch folder of its own (AGENTS.md, "Shell hygiene"), its runtime
//! folder among them: no test ever reaches its user's own engine, nor
//! reads their home.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use skwird_client::Client;
use skwird_protocol::{Links, Match, NameTerm, Proof, Query, Why, WirePath};
use skwird_testkit::{Scratch, reference_walk};

/// A scratch home, its XDG folders, and the engines run in it.
struct Env {
    s: Scratch,
    engines: Vec<Child>,
}

impl Env {
    fn new(label: &str) -> Env {
        let s = Scratch::new(label);
        for d in ["home", "run", "cache", "state", "config"] {
            s.dir(d);
        }
        Env {
            s,
            engines: Vec::new(),
        }
    }

    fn home(&self) -> PathBuf {
        self.s.join("home")
    }

    fn socket(&self) -> PathBuf {
        self.s.join("run/skwird/socket")
    }

    /// The program, with every folder it may reach this test's own.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_skwird"));
        c.args(args)
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.s.join("run"))
            .env("XDG_CACHE_HOME", self.s.join("cache"))
            .env("XDG_STATE_HOME", self.s.join("state"))
            .env("XDG_CONFIG_HOME", self.s.join("config"))
            // An engine a client of the test starts goes soon after.
            .env("SKWIRD_IDLE", "1");
        c
    }

    /// An engine of the home, gone after `idle` seconds without a client,
    /// its log in the test's folder ([`Env::log`]).
    fn engine(&mut self, idle: u64) {
        let home = self.home();
        let idle = idle.to_string();
        let log = std::fs::File::create(self.s.join(format!("engine-{}.log", self.engines.len())))
            .unwrap();
        let child = self
            .command(&[
                "serve",
                "--root",
                home.to_str().unwrap(),
                "--idle",
                &idle,
                "--threads",
                "2",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.engines.push(child);
    }

    /// What every engine of the test logged.
    fn log(&self) -> String {
        (0..self.engines.len())
            .map(|i| {
                std::fs::read_to_string(self.s.join(format!("engine-{i}.log"))).unwrap_or_default()
            })
            .collect()
    }

    /// A client of the engine, once it answers and its index is ready.
    fn client(&self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(mut c) = Client::at(&self.socket(), "macos-test") {
                let ready = c
                    .coverage(&self.home())
                    .is_ok_and(|cov| cov.names != Proof::NotCovered(Why::Unread));
                if ready {
                    return c;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the engine never answered:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for e in &mut self.engines {
            let _ = e.kill();
            let _ = e.wait();
        }
    }
}

fn everything(root: &Path) -> Query {
    Query {
        root: WirePath::of(root),
        name: None,
        kinds: None,
        size: None,
        mtime: None,
        limit: None,
        links: Links::Own,
    }
}

fn named(root: &Path, text: &str) -> Query {
    Query {
        name: Some(NameTerm {
            how: Match::Skwir,
            text: text.to_owned(),
            case_sensitive: false,
        }),
        ..everything(root)
    }
}

/// Every path the engine answers `query` with.
fn answered(c: &mut Client, query: &Query) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    c.query(query, |batch| {
        paths.extend(batch.iter().map(|e| e.path.to_path()));
        true
    })
    .unwrap();
    paths
}

/// Every path a walk finds below `root`.
fn walked(root: &Path) -> BTreeSet<PathBuf> {
    reference_walk(root)
        .entries
        .into_keys()
        .map(|p| root.join(p))
        .collect()
}

/// A home of reports, notes, and names in Unicode: one composed, one with
/// an accent decomposed (APFS keeps each as it was written).
fn tree(env: &Env) {
    let home = env.home();
    for i in 0..20 {
        env.s
            .file(home.join(format!("docs/d{i:02}/report {i}.txt")));
    }
    env.s.file(home.join("notes.md"));
    env.s.file(home.join("été/naïve 東京 🙂.md"));
    env.s.file(home.join("cafe\u{301}.txt"));
}

/// The engine answers what a walk finds, Unicode names among them, as it
/// read them; a path outside its root is said so.
#[test]
fn an_engine_answers_what_a_walk_finds() {
    let mut env = Env::new("mac-answers");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    assert_eq!(c.welcome().roots, vec![WirePath::of(&home)]);
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
    assert_eq!(
        answered(&mut c, &named(&home, "東京")),
        [home.join("été/naïve 東京 🙂.md")].into()
    );
    assert_eq!(
        answered(&mut c, &named(&home, "cafe")),
        [home.join("cafe\u{301}.txt")].into(),
        "the name as written"
    );
    assert_eq!(
        answered(&mut c, &named(&home.join("docs"), "report")).len(),
        20
    );
    // Heard whole once read (step 3), or checked until then.
    let coverage = c.coverage(&home).unwrap();
    assert!(
        matches!(coverage.names, Proof::Live(_) | Proof::Checked(_)),
        "{coverage:?}"
    );
    assert_eq!(
        c.coverage(Path::new("/elsewhere")).unwrap().names,
        Proof::NotCovered(Why::Outside)
    );
}

/// A change made behind the engine's back is in the answer after a sync:
/// live, its cookie heard after the change, or checked at a time after the
/// sync was asked.
#[test]
fn a_sync_answers_with_a_change_made_behind_the_engines_back() {
    let mut env = Env::new("mac-sync");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    let made = env.s.file(home.join("docs/d07/made behind its back.txt"));
    let asked = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let coverage = c.sync(&home).unwrap();
    match coverage.names {
        Proof::Checked(at) => assert!(
            at.sec >= i64::try_from(asked.as_secs()).unwrap() - 1,
            "{at:?}"
        ),
        Proof::Live(_) => {}
        _ => panic!("{coverage:?}"),
    }
    assert!(answered(&mut c, &named(&home, "behind its back")).contains(&made));
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
}

/// A client that finds no engine starts it, and is answered by it, on the
/// socket in the runtime folder it gave; with every client gone, the
/// engine goes, and its socket with it.
#[test]
fn a_client_starts_the_engine_where_none_answers() {
    let env = Env::new("mac-started");
    tree(&env);
    let out = env.command(&["status"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains(&format!("root: {}", env.home().display())),
        "{said}"
    );
    let mut c = env.client();
    assert!(!answered(&mut c, &named(&env.home(), "notes")).is_empty());
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(20);
    while env.socket().exists() {
        assert!(
            Instant::now() < deadline,
            "the engine a client started stayed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The home's private folders are kept out of the index, macOS' keychains
/// among them: not covered, excluded, none of their names answered.
#[test]
fn the_private_defaults_hold() {
    let mut env = Env::new("mac-private");
    tree(&env);
    let home = env.home();
    env.s.file(home.join(".ssh/id_test"));
    env.s.file(home.join("Library/Keychains/login.keychain-db"));
    env.engine(30);
    let mut c = env.client();
    for name in ["id_test", "login.keychain"] {
        assert!(answered(&mut c, &named(&home, name)).is_empty(), "{name}");
    }
    let holes: Vec<(PathBuf, Why)> = c
        .coverage(&home)
        .unwrap()
        .holes
        .into_iter()
        .map(|h| (h.path.to_path(), h.why))
        .collect();
    for private in [".ssh", "Library/Keychains"] {
        assert!(
            holes.contains(&(home.join(private), Why::Excluded)),
            "{private}: {holes:?}"
        );
    }
}

/// Wait until `done` holds, up to `seconds`; or say what `env` logged.
fn wait_for(env: &Env, seconds: u64, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "{what}:\n{}", env.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn is_live(c: &mut Client, path: &Path) -> bool {
    matches!(c.coverage(path).unwrap().names, Proof::Live(_))
}

/// The root is heard whole (step 3): live with no folder registered, and
/// a change made in it, deep or new, is answered without a sync, within
/// the stream's latency and the gathering's second; a removal as soon.
#[test]
fn a_change_is_answered_live_without_a_sync() {
    let mut env = Env::new("mac-live");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    wait_for(&env, 10, "live", || is_live(&mut c, &home));
    assert!(is_live(&mut c, &home.join("docs/d05")));
    let made = env.s.file(home.join("docs/d05/made live.txt"));
    let deep = env.s.file(home.join("new/folder/deep inside.txt"));
    wait_for(&env, 5, "the changes answered without a sync", || {
        let found = answered(&mut c, &named(&home, "live.txt"));
        found.contains(&made) && answered(&mut c, &named(&home, "deep inside")).contains(&deep)
    });
    env.s.remove("home/docs/d05/made live.txt");
    wait_for(&env, 5, "a removal answered without a sync", || {
        answered(&mut c, &named(&home, "made live")).is_empty()
    });
    assert!(is_live(&mut c, &home));
}

/// A sync is a barrier: its cookie, made in the roots' stream, is heard
/// after a change made just before it, and the sync answers live with the
/// change, well before it would give up on the cookie (2 s) and check.
#[test]
fn a_sync_is_a_barrier() {
    let mut env = Env::new("mac-barrier");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    wait_for(&env, 10, "live", || is_live(&mut c, &home));
    for i in 0..5 {
        let made = env
            .s
            .file(home.join(format!("docs/d07/just before {i}.txt")));
        let begun = Instant::now();
        let synced = c.sync(&home).unwrap();
        let took = begun.elapsed();
        assert!(matches!(synced.names, Proof::Live(_)), "{synced:?}");
        assert!(
            took < Duration::from_millis(1500),
            "its cookie came late: {took:?}"
        );
        assert!(
            answered(&mut c, &named(&home, &format!("just before {i}"))).contains(&made),
            "sync {i}"
        );
    }
}

/// A root moved away is no longer heard (its stream says so): its answers
/// are not live from then on.
#[test]
fn a_root_moved_away_is_not_live() {
    let mut env = Env::new("mac-moved");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    wait_for(&env, 10, "live", || is_live(&mut c, &home));
    env.s.rename("home", "elsewhere");
    wait_for(&env, 5, "not live once moved", || !is_live(&mut c, &home));
}

/// An engine that went, started again, resumes from the position its
/// index kept: what changed while none ran is heard as its history is
/// replayed, and the root is live once it is, with no check (DESIGN 2).
#[test]
fn a_restarted_engine_resumes_from_its_history() {
    let mut env = Env::new("mac-resume");
    tree(&env);
    let home = env.home();
    env.engine(2);
    {
        let mut c = env.client();
        wait_for(&env, 10, "live", || is_live(&mut c, &home));
        // A change heard after the build, so that a position is kept.
        let heard = env.s.file(home.join("docs/d03/heard live.txt"));
        wait_for(&env, 5, "the change answered", || {
            answered(&mut c, &named(&home, "heard live")).contains(&heard)
        });
    }
    wait_for(&env, 20, "the engine went", || !env.socket().exists());
    let first = env.log();
    assert!(
        first.contains("root 0: its history's position kept"),
        "{first}"
    );
    let made = env.s.file(home.join("docs/d04/made while none ran.txt"));
    env.engine(30);
    let mut c = env.client();
    wait_for(&env, 10, "live", || is_live(&mut c, &home));
    let synced = c.sync(&home).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{synced:?}");
    assert!(
        answered(&mut c, &named(&home, "while none ran")).contains(&made),
        "{}",
        env.log()
    );
    let second = env.log()[first.len()..].to_owned();
    assert!(
        second.contains("root 0: resumed from its history's position kept"),
        "{second}"
    );
    assert!(
        second.contains("root 0: its history replayed, live"),
        "{second}"
    );
    assert!(!second.contains("root 0 checked"), "{second}");
}

/// The folders macOS asks consent for, in the home, are never opened
/// without Full Disk Access: not covered, as denied, none of their names
/// answered, and the engine says so. With it, they are read as any.
#[test]
fn folders_asked_consent_for_are_read_with_full_disk_access_alone() {
    let mut env = Env::new("mac-consent");
    tree(&env);
    let home = env.home();
    let desktop = env.s.file(home.join("Desktop/on the desktop.txt"));
    env.s
        .file(home.join("Library/Containers/an app/its own.txt"));
    env.engine(30);
    let mut c = env.client();
    let found = answered(&mut c, &named(&home, "on the desktop"));
    let holes: Vec<(PathBuf, Why)> = c
        .coverage(&home)
        .unwrap()
        .holes
        .into_iter()
        .map(|h| (h.path.to_path(), h.why))
        .collect();
    if skwird_crawl::full_disk_access() {
        eprintln!("this machine gives Full Disk Access: the folders are read");
        assert!(found.contains(&desktop), "{found:?}");
        assert!(!answered(&mut c, &named(&home, "its own")).is_empty());
    } else {
        assert!(found.is_empty(), "{found:?}");
        assert!(answered(&mut c, &named(&home, "its own")).is_empty());
        for asked in ["Desktop", "Library/Containers"] {
            assert!(
                holes.contains(&(home.join(asked), Why::Denied)),
                "{asked}: {holes:?}"
            );
        }
        assert!(
            env.log()
                .contains("folders macOS asks consent for are not read"),
            "{}",
            env.log()
        );
    }
}

/// `skwird logon add` writes the user's `LaunchAgent` and has launchd take
/// it, which starts the engine as it would at login; `remove` takes it
/// back. On a machine thrown away after its tests alone: launchd starts
/// that engine with the user's own home, not the test's.
#[test]
fn the_login_agent_is_written_and_taken() {
    if !skwird_testkit::throwaway() {
        eprintln!(
            "skipped: it would have launchd start an engine of the user running it (SKWIRD_TEST_MACHINE)"
        );
        return;
    }
    let env = Env::new("mac-agent");
    let run = |what: &str| {
        let out = env.command(&["logon", what]).output().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run("add");
    let file = env
        .home()
        .join("Library/LaunchAgents/org.skwir.skwird.plist");
    let plist = std::fs::read_to_string(&file).unwrap();
    assert!(plist.contains("<string>--forever</string>"), "{plist}");
    let uid = rustix::process::getuid().as_raw();
    let print = || {
        Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/org.skwir.skwird")])
            .output()
            .unwrap()
    };
    let out = print();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("serve"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    run("remove");
    assert!(!print().status.success(), "the agent is taken back");
    assert!(!file.exists());
}
