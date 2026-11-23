//! A user's engine on Windows, run as its program and asked over its pipe
//! (`docs/PLAN.md`, phase 4, step 4), under Wine in the gate
//! (`tools/wine-tests`).
//!
//! Every engine a test runs is given a home, an application data folder
//! and a pipe of the test's own (`USERPROFILE`, `LOCALAPPDATA`,
//! `SKWIRD_PIPE`), as Linux's tests give their XDG folders: no test ever
//! reaches its user's own engine, nor reads their home.

#![cfg(windows)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use skwird_client::{Client, Find};
use skwird_protocol::{Match, NameTerm, Proof, Query, Why, WirePath};
use skwird_testkit::{Scratch, reference_walk};

/// The program. Built from Linux for Windows, cargo names it by the
/// host's path; under Wine, the host's root is drive `Z:`.
fn program() -> PathBuf {
    let built = env!("CARGO_BIN_EXE_skwird");
    if built.starts_with('/') {
        PathBuf::from(format!("Z:{}", built.replace('/', "\\")))
    } else {
        PathBuf::from(built)
    }
}

/// A scratch home, its application data, its pipe, and the engines run
/// on them.
struct Env {
    s: Scratch,
    pipe: PathBuf,
    engines: Vec<Child>,
}

impl Env {
    fn new(label: &str) -> Env {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let s = Scratch::new(label);
        s.dir("home");
        s.dir("local");
        let pipe = PathBuf::from(format!(
            r"\\.\pipe\skwird-test-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Env {
            s,
            pipe,
            engines: Vec::new(),
        }
    }

    fn home(&self) -> PathBuf {
        self.s.join("home")
    }

    /// The program, with every folder it may reach this test's own.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(program());
        c.args(args)
            .env("USERPROFILE", self.home())
            .env("LOCALAPPDATA", self.s.join("local"))
            .env("SKWIRD_PIPE", &self.pipe)
            // An engine a client of the test starts goes soon after.
            .env("SKWIRD_IDLE", "1");
        c
    }

    /// An engine of the home, gone after `idle` seconds without a client.
    fn engine(&mut self, idle: u64) -> usize {
        let home = self.home();
        let idle = idle.to_string();
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
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.engines.push(child);
        self.engines.len() - 1
    }

    fn find(&self) -> Find {
        Find {
            socket: Some(self.pipe.clone()),
            start: None,
            ..Find::new("a test")
        }
    }

    /// A client of the engine, once it answers and its index is ready.
    fn client(&self) -> Client {
        let home = self.home();
        wait_for(20, "the engine never answered", || {
            Client::connect(&self.find()).is_ok_and(|mut c| {
                c.coverage(&home)
                    .is_ok_and(|cov| cov.names != Proof::NotCovered(Why::Unread))
            })
        });
        Client::connect(&self.find()).unwrap()
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

/// Wait until `done` holds, up to `seconds`.
fn wait_for(seconds: u64, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
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

/// The subtrees not covered below `root`, with why.
fn holes(c: &mut Client, root: &Path) -> Vec<(PathBuf, Why)> {
    let mut holes: Vec<_> = c
        .coverage(root)
        .unwrap()
        .holes
        .into_iter()
        .map(|h| (h.path.to_path(), h.why))
        .collect();
    holes.sort_by(|a, b| a.0.cmp(&b.0));
    holes
}

fn tree(env: &Env) {
    let home = env.home();
    for i in 0..20 {
        env.s
            .file(home.join(format!("docs/d{i:02}/report {i}.txt")));
    }
    env.s.file(home.join("notes.md"));
    env.s.file(home.join("été/naïve 東京 🙂.md"));
}

/// The engine answers what a walk finds, Unicode names among them, its
/// names checked; a path outside its root is said so.
#[test]
fn an_engine_answers_what_a_walk_finds() {
    let mut env = Env::new("win-answers");
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
        answered(&mut c, &named(&home.join("docs"), "report")).len(),
        20
    );
    let coverage = c.coverage(&home).unwrap();
    assert!(matches!(coverage.names, Proof::Checked(_)), "{coverage:?}");
    assert_eq!(
        c.coverage(Path::new(r"C:\elsewhere")).unwrap().names,
        Proof::NotCovered(Why::Outside)
    );
}

/// A change made behind the engine's back is in the answer after a sync,
/// checked at a time after the sync was asked.
#[test]
fn a_sync_answers_with_a_change_made_behind_the_engines_back() {
    let mut env = Env::new("win-sync");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    let made = env.s.file(home.join("docs/d07/made behind its back.txt"));
    let asked = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let coverage = c.sync(&home).unwrap();
    let Proof::Checked(at) = coverage.names else {
        panic!("{coverage:?}")
    };
    assert!(
        at.sec >= i64::try_from(asked.as_secs()).unwrap() - 1,
        "{at:?}"
    );
    assert!(answered(&mut c, &named(&home, "behind its back")).contains(&made));
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
}

/// A client that finds no engine starts it, and is answered by it; with
/// every client gone, the engine goes, and its pipe with it. The engine
/// holds none of the client's handles: the client's output ends when the
/// client does, not when the engine it started goes. Before, the engine
/// inherited the pipe a test read the client's output from, and that
/// read waited for the engine to go idle.
#[test]
fn a_client_starts_the_engine_where_none_answers() {
    let env = Env::new("win-started");
    tree(&env);
    // The program as a client: it starts itself, on this test's folders,
    // to stay 5 s once idle. Its output read through a pipe ends when it
    // does, not when the engine it started goes. Wine starts each process
    // as one of its host's, which inherits every descriptor its parent
    // holds open whatever Windows was asked: under Wine, such a read waits
    // for the engine, so the output goes to a file, and the client's end
    // alone is waited for.
    let status = || {
        let mut c = env.command(&["status"]);
        c.env("SKWIRD_IDLE", "5");
        c
    };
    let begun = Instant::now();
    let (done, said) = if skwird_testkit::under_wine() {
        let file = env.s.join("said.txt");
        let done = status()
            .stdout(std::fs::File::create(&file).unwrap())
            .status()
            .unwrap();
        eprintln!("not proven under Wine: the client's output ends with it");
        (done, std::fs::read_to_string(&file).unwrap())
    } else {
        let out = status().output().unwrap();
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let took = begun.elapsed();
    assert!(done.success(), "{said}");
    assert!(
        said.contains(&format!("root: {}", env.home().display())),
        "{said}"
    );
    assert!(took < Duration::from_secs(5), "the client held: {took:?}");
    let mut c = env.client();
    assert!(!answered(&mut c, &named(&env.home(), "notes")).is_empty());
    drop(c);
    // It logs where the client named, with none of its handles; and says
    // there when it goes. Asked whether it is gone, it would stay: each
    // connection is a client.
    let log = env.s.join(r"local\skwird\engine.log");
    wait_for(20, "the engine a client started stayed", || {
        std::fs::read_to_string(&log).is_ok_and(|l| l.contains("idle: gone"))
    });
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(log.contains("serving 1 roots"), "{log}");
    assert!(Client::connect(&env.find()).is_err(), "its pipe gone");
}

/// The user's file of exclusions, and the default private folders of a
/// home on Windows, are kept out of the index: not covered, excluded.
#[test]
fn the_users_exclusions_and_the_private_defaults_hold() {
    let mut env = Env::new("win-excluded");
    tree(&env);
    let home = env.home();
    env.s.file(home.join(".ssh/id_ed25519"));
    env.s
        .file(home.join("AppData/Roaming/Microsoft/Credentials/0A1B2C3D"));
    env.s.write(
        env.s.join(r"local\skwird\config\exclude"),
        b"# kept out\n~/docs/d03\n",
    );
    env.engine(30);
    let mut c = env.client();
    assert_eq!(
        holes(&mut c, &home),
        vec![
            (home.join(".ssh"), Why::Excluded),
            (
                home.join(r"AppData\Roaming\Microsoft\Credentials"),
                Why::Excluded
            ),
            (home.join(r"docs\d03"), Why::Excluded),
        ]
    );
    assert!(answered(&mut c, &named(&home, "id_ed25519")).is_empty());
    assert!(answered(&mut c, &named(&home, "report 3.txt")).is_empty());
    assert_eq!(answered(&mut c, &named(&home, "report 4.txt")).len(), 1);
}

/// One engine per user: a second waits on the lock while the first
/// serves, and takes over once it is gone.
#[test]
fn a_second_engine_waits_and_takes_over() {
    let mut env = Env::new("win-second");
    tree(&env);
    let first = env.engine(30);
    let mut c = env.client();
    let second = env.engine(30);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        env.engines[second].try_wait().unwrap().is_none(),
        "the second waits"
    );
    assert!(!answered(&mut c, &named(&env.home(), "notes")).is_empty());
    drop(c);
    env.engines[first].kill().unwrap();
    env.engines[first].wait().unwrap();
    let mut c = env.client();
    assert!(!answered(&mut c, &named(&env.home(), "notes")).is_empty());
    assert!(
        env.engines[second].try_wait().unwrap().is_none(),
        "the second serves"
    );
}
