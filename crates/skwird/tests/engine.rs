//! A user's engine, run as its program and asked through the client
//! (`docs/PLAN.md`, phase 1, step 5).
//!
//! Every process a test runs is given `HOME` and the XDG folders of a
//! scratch folder of its own (AGENTS.md, "Shell hygiene"): no test ever
//! reaches its user's own engine, nor crawls their home.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use skwird_client::{Client, Find};
use skwird_protocol::{Match, NameTerm, Proof, Query, Why, WirePath};
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
    fn command(&self, args: &[&OsStr]) -> Command {
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

    /// An engine of the home, gone after `idle` seconds without a client.
    fn engine(&mut self, idle: u64) -> usize {
        let idle = idle.to_string();
        let child = self
            .command(&[
                OsStr::new("serve"),
                OsStr::new("--root"),
                self.home().as_os_str(),
                OsStr::new("--idle"),
                OsStr::new(&idle),
                OsStr::new("--threads"),
                OsStr::new("2"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.engines.push(child);
        self.engines.len() - 1
    }

    /// A client of the engine, once it answers and its index is ready.
    fn client(&self, name: &str) -> Client {
        connect(&self.socket(), &self.home(), name)
    }
}

/// A client of the engine at `socket`, once it answers and its index of
/// `home` is ready.
fn connect(socket: &Path, home: &Path, name: &str) -> Client {
    {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(mut c) = Client::at(socket, name) {
                let ready = c
                    .coverage(home)
                    .is_ok_and(|cov| cov.names != Proof::NotCovered(Why::Unread));
                if ready {
                    return c;
                }
            }
            assert!(Instant::now() < deadline, "the engine never answered");
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

fn tree(env: &Env) {
    let home = env.home();
    for i in 0..40 {
        env.s
            .file(home.join(format!("docs/d{i:02}/report {i}.txt")));
    }
    env.s.file(home.join("notes.md"));
    env.s
        .file(home.join(Path::new(OsStr::from_bytes(b"latin1-\xe9t\xe9"))));
}

/// The engine answers what a walk finds, names that are not UTF-8 byte
/// for byte, its names checked.
#[test]
fn an_engine_answers_what_a_walk_finds() {
    let mut env = Env::new("engine-answers");
    tree(&env);
    env.engine(30);
    let mut c = env.client("a test");
    assert_eq!(c.welcome().roots, vec![WirePath::of(&env.home())]);
    let home = env.home();
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
    let latin = home.join(Path::new(OsStr::from_bytes(b"latin1-\xe9t\xe9")));
    assert!(
        answered(&mut c, &named(&home, "latin1")).contains(&latin),
        "byte for byte"
    );
    let reports = answered(&mut c, &named(&home.join("docs"), "report"));
    assert_eq!(reports.len(), 40);
    let coverage = c.coverage(&home).unwrap();
    assert!(matches!(coverage.names, Proof::Checked(_)), "{coverage:?}");
    assert_eq!(
        c.coverage(Path::new("/elsewhere")).unwrap().names,
        Proof::NotCovered(Why::Outside)
    );
}

/// A change made behind the engine's back is in the answer after a sync,
/// and its coverage is as of a time after the sync was asked.
#[test]
fn a_sync_answers_with_a_change_made_behind_the_engines_back() {
    let mut env = Env::new("engine-sync");
    tree(&env);
    env.engine(30);
    let mut c = env.client("a test");
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

/// Queries made at once, by clients of their own, answer as queries made
/// one by one.
#[test]
fn queries_at_once_answer_as_queries_one_by_one() {
    let mut env = Env::new("engine-at-once");
    tree(&env);
    env.engine(30);
    let home = env.home();
    let queries: Vec<Query> = ["report", "1", "notes", "zz", ".txt", "d0"]
        .iter()
        .map(|t| named(&home, t))
        .chain([everything(&home)])
        .collect();
    let mut one = env.client("one by one");
    let alone: Vec<_> = queries.iter().map(|q| answered(&mut one, q)).collect();
    let socket = env.socket();
    let together: Vec<Vec<_>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..6)
            .map(|_| {
                scope.spawn(|| {
                    let mut c = connect(&socket, &home, "at once");
                    queries
                        .iter()
                        .map(|q| answered(&mut c, q))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    for answers in together {
        assert_eq!(answers, alone);
    }
}

/// A second engine waits on the lock while the first holds it; the first
/// gone during its first crawl, the second takes over, and answers what a
/// walk finds.
#[test]
fn a_second_engine_takes_over_when_the_first_exits_during_its_first_crawl() {
    let mut env = Env::new("engine-takeover");
    let home = env.home();
    for i in 0..3000 {
        env.s
            .file(home.join(format!("many/f{:02}/file {i}", i % 60)));
    }
    let first = env.engine(30);
    let second = env.engine(30);
    std::thread::sleep(Duration::from_millis(30));
    env.engines[first].kill().unwrap();
    env.engines[first].wait().unwrap();
    let mut c = env.client("after the first");
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
    assert!(
        env.engines[second].try_wait().unwrap().is_none(),
        "the second serves"
    );
}

/// A socket whose path is longer than a Unix socket takes is an error
/// that says so.
#[test]
fn a_socket_path_too_long_is_an_error_that_says_so() {
    let env = Env::new("engine-long");
    let deep = env
        .s
        .dir(format!("run/{}", "a-long-folder-name/".repeat(6)));
    let out = env
        .command(&[
            OsStr::new("serve"),
            OsStr::new("--root"),
            env.home().as_os_str(),
        ])
        .env("XDG_RUNTIME_DIR", &deep)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("past the 107"), "{said}");
}

/// Two clients register folders, and each takes back only its own: a
/// folder registered is a hole at once, its names gone; taken back by its
/// client, it is read again.
#[test]
fn two_clients_register_folders_and_each_takes_back_only_its_own() {
    let mut env = Env::new("engine-register");
    tree(&env);
    env.engine(30);
    let home = env.home();
    let (x, y) = (home.join("docs/d01"), home.join("docs/d02"));
    let mut a = env.client("client a");
    let mut b = env.client("client b");
    assert_eq!(a.exclude(&[&x], &[]).unwrap(), vec![x.clone()]);
    assert_eq!(b.exclude(&[&y], &[]).unwrap(), vec![y.clone()]);
    let holes = |c: &mut Client| -> BTreeSet<PathBuf> {
        c.coverage(&home)
            .unwrap()
            .holes
            .into_iter()
            .filter(|h| h.why == Why::Excluded)
            .map(|h| h.path.to_path())
            .collect()
    };
    assert_eq!(holes(&mut a), BTreeSet::from([x.clone(), y.clone()]));
    assert!(
        answered(&mut a, &named(&home, "report 1.txt")).is_empty(),
        "its names gone"
    );
    // b cannot take back what a registered.
    assert_eq!(b.exclude(&[], &[&x]).unwrap(), vec![y.clone()]);
    assert_eq!(holes(&mut b), BTreeSet::from([x.clone(), y.clone()]));
    let left = a.exclude(&[], &[&x]).unwrap();
    assert!(left.is_empty(), "{left:?}");
    a.sync(&home).unwrap();
    assert_eq!(holes(&mut a), BTreeSet::from([y.clone()]));
    assert!(
        !answered(&mut a, &named(&home, "report 1.txt")).is_empty(),
        "read again"
    );
}

/// An engine goes once its last client has been gone its idle time.
#[test]
fn an_engine_goes_once_idle() {
    let mut env = Env::new("engine-idle");
    tree(&env);
    let engine = env.engine(1);
    drop(env.client("a test"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.engines[engine].try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the engine stayed");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!env.socket().exists(), "its socket let go of");
}

/// A client that finds no engine starts it, and is answered by it.
#[test]
fn a_client_starts_the_engine_where_none_answers() {
    let env = Env::new("engine-started");
    tree(&env);
    // The program as a client: it starts itself, with this test's folders.
    let out = env.command(&[OsStr::new("status")]).output().unwrap();
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
    // The engine it started answers a client of this test, then is let go.
    let mut c = env.client("a test");
    assert!(!answered(&mut c, &named(&env.home(), "notes")).is_empty());
    let _ = env.command(&[OsStr::new("status")]).output();
    drop(c);
    let started = Find {
        socket: Some(env.socket()),
        start: None,
        ..Find::new("a test")
    };
    assert!(Client::connect(&started).is_ok());
    // Every client gone, the engine it started goes too.
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.socket().exists() {
        assert!(
            Instant::now() < deadline,
            "the engine a client started stayed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A query cancelled after its first batch says so, and stops there.
#[test]
fn a_cancelled_query_says_so_and_stops() {
    let mut env = Env::new("engine-cancel");
    let home = env.home();
    for i in 0..3000 {
        env.s
            .file(home.join(format!("many/f{:02}/file {i}", i % 30)));
    }
    env.engine(30);
    let mut c = env.client("a test");
    let mut batches = 0;
    let done = c
        .query(&everything(&home), |_| {
            batches += 1;
            false
        })
        .unwrap();
    assert!(done.cancelled, "{done:?}");
    assert!(done.count < 3000, "{done:?}");
    assert_eq!(batches, 1, "no batch handed after the cancel");
    // The connection goes on: the next query is answered whole.
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
}
