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
use skwird_testkit::{Scratch, in_namespace, reference_walk};

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
        self.engine_with(idle, &[])
    }

    /// An engine of the home, its environment given `env` too.
    fn engine_with(&mut self, idle: u64, env: &[(&str, &str)]) -> usize {
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
            .envs(env.iter().copied())
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

/// The holes the engine answers below `root`, with why.
fn holes_of(c: &mut Client, root: &Path) -> Vec<(PathBuf, Why)> {
    c.coverage(root)
        .unwrap()
        .holes
        .into_iter()
        .map(|h| (h.path.to_path(), h.why))
        .collect()
}

/// A folder the user names in their exclusions is purged from the next
/// sync, or, written while no engine ran, before the next start answers
/// anything, though its parent never changed. Taken out while no engine
/// ran, it is read again. Each engine is killed, never left to save.
#[test]
fn the_users_exclusions_hold_from_the_next_sync_and_the_next_start() {
    let mut env = Env::new("engine-configured");
    tree(&env);
    // Past the check's tick, so that no folder is read again for having
    // changed just before it was read: only the exclusions can purge.
    std::thread::sleep(Duration::from_millis(2200));
    let home = env.home();
    let excluded = home.join("docs/d03");
    let file = env.s.join("config/skwird/exclude");
    let report = named(&home, "report 3.txt");
    let kill = |env: &mut Env, engine: usize| {
        env.engines[engine].kill().unwrap();
        env.engines[engine].wait().unwrap();
    };
    let first = env.engine(30);
    let mut c = env.client("a test");
    assert_eq!(answered(&mut c, &report).len(), 1);
    // Written while it runs: it holds from the next sync.
    env.s.write(&file, b"# kept out\n~/docs/d03\n");
    c.sync(&home).unwrap();
    assert!(answered(&mut c, &report).is_empty(), "its names gone");
    assert_eq!(holes_of(&mut c, &home), [(excluded.clone(), Why::Excluded)]);
    drop(c);
    kill(&mut env, first);
    // Taken out while no engine ran: not covered until read again.
    env.s.write(&file, b"");
    let second = env.engine(30);
    let mut c = env.client("a test");
    assert_eq!(holes_of(&mut c, &home), [(excluded.clone(), Why::Unread)]);
    c.sync(&home).unwrap();
    assert_eq!(holes_of(&mut c, &home), []);
    assert_eq!(answered(&mut c, &report).len(), 1, "read again");
    drop(c);
    kill(&mut env, second);
    // Written while no engine ran: it holds before any sync.
    env.s.write(&file, b"~/docs/d03/\n");
    env.engine(30);
    let mut c = env.client("a test");
    assert!(answered(&mut c, &report).is_empty(), "its names gone");
    assert_eq!(holes_of(&mut c, &home), [(excluded, Why::Excluded)]);
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

/// Wait until `done` holds, up to `seconds`.
fn wait_for(seconds: u64, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_live(c: &mut Client, path: &Path) -> bool {
    matches!(c.coverage(path).unwrap().names, Proof::Live(_))
}

/// A folder a client watches is live, and a change made in it is
/// answered without a sync, within the gathering's second; the rest of
/// the home stays checked.
#[test]
fn a_watched_folder_is_answered_live_without_a_sync() {
    let mut env = Env::new("engine-live");
    tree(&env);
    env.engine(30);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    assert_eq!(c.watch(&[&docs], &[]).unwrap(), vec![docs.clone()]);
    assert!(is_live(&mut c, &docs));
    assert!(is_live(&mut c, &docs.join("d05")));
    assert!(!is_live(&mut c, &home), "the home is checked only");
    let made = env.s.file(docs.join("d05/made live.txt"));
    let asked = Instant::now();
    wait_for(5, "the change answered", || {
        answered(&mut c, &named(&home, "made live")).contains(&made)
    });
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "{:?}",
        asked.elapsed()
    );
}

/// A folder made, filled and moved within a watched folder is answered
/// live: its new folders watched before they are read, nothing left
/// under its old path.
#[test]
fn a_folder_made_filled_and_moved_is_answered_live() {
    let mut env = Env::new("engine-live-moved");
    tree(&env);
    env.engine(30);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    env.s.file(docs.join("new/a/b/first.txt"));
    wait_for(5, "the new folder crawled", || {
        answered(&mut c, &named(&home, "first.txt")).contains(&docs.join("new/a/b/first.txt"))
    });
    env.s.file(docs.join("new/a/b/later.txt"));
    wait_for(5, "a file made in a folder crawled since", || {
        answered(&mut c, &named(&home, "later.txt")).contains(&docs.join("new/a/b/later.txt"))
    });
    env.s.rename(docs.join("new"), docs.join("moved"));
    wait_for(5, "the move answered", || {
        let got = answered(&mut c, &named(&home, "later.txt"));
        got.contains(&docs.join("moved/a/b/later.txt"))
            && !got.contains(&docs.join("new/a/b/later.txt"))
    });
    assert!(is_live(&mut c, &docs));
}

/// Events dropped while the engine could not read them (stopped while
/// more files were made than the queue holds): coverage is lowered at
/// once, then the check after the quiet brings "live" back, with every
/// file answered.
#[test]
fn an_overflow_lowers_coverage_and_the_check_after_brings_it_back() {
    let mut env = Env::new("engine-live-overflow");
    tree(&env);
    let engine = env.engine(60);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    let most: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = env.engines[engine].id().to_string();
    let signal = |which: &str| {
        assert!(
            Command::new("kill")
                .args([which, pid.as_str()])
                .status()
                .unwrap()
                .success()
        );
    };
    // Made and watched first: what is made in it then is heard, queued.
    let busy = env.s.dir(docs.join("busy"));
    env.s.file(busy.join("first"));
    wait_for(5, "the busy folder watched", || {
        answered(&mut c, &named(&home, "first")).contains(&busy.join("first"))
    });
    signal("-STOP");
    for i in 0..most + 64 {
        std::fs::write(busy.join(format!("f{i}")), b"").unwrap();
    }
    signal("-CONT");
    wait_for(5, "coverage lowered", || !is_live(&mut c, &docs));
    wait_for(20, "live again", || is_live(&mut c, &docs));
    let all = Query {
        limit: None,
        ..named(&busy, "f")
    };
    let got = answered(&mut c, &all);
    let missing = (0..most + 64)
        .filter(|i| !got.contains(&busy.join(format!("f{i}"))))
        .count();
    assert_eq!(missing, 0, "files missing after the check");
    assert!(got.contains(&busy.join("first")));
}

/// Past the engine's budget of watches, a watched folder is checked, not
/// live; taken back, it lets go of its watches.
#[test]
fn a_folder_past_the_budget_is_checked_not_live() {
    let mut env = Env::new("engine-live-budget");
    tree(&env);
    env.engine_with(30, &[("SKWIRD_WATCHES", "5")]);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    let coverage = c.coverage(&docs).unwrap();
    assert!(matches!(coverage.names, Proof::Checked(_)), "{coverage:?}");
    let few = home.join("docs/d01");
    c.watch(&[], &[&docs]).unwrap();
    c.watch(&[&few], &[]).unwrap();
    assert!(is_live(&mut c, &few), "a folder within the budget is live");
}

/// What a client watches is kept across a restart: the next engine
/// watches it before any client asks again. Taken back, it is checked.
#[test]
fn watched_folders_are_kept_across_a_restart() {
    let mut env = Env::new("engine-live-kept");
    tree(&env);
    let first = env.engine(30);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    drop(c);
    env.engines[first].kill().unwrap();
    env.engines[first].wait().unwrap();
    env.engine(30);
    let mut c = env.client("a test");
    wait_for(5, "live after the restart", || is_live(&mut c, &docs));
    assert_eq!(c.watch(&[], &[&docs]).unwrap(), Vec::<PathBuf>::new());
    assert!(!is_live(&mut c, &docs));
}

/// A sync of a live folder answers with a change made just before it: the
/// barrier's cookie, heard in its turn, says every event before it was
/// read, and what was gathered is applied before the answer.
#[test]
fn a_sync_of_a_live_folder_answers_with_a_change_made_just_before_it() {
    let mut env = Env::new("engine-barrier");
    tree(&env);
    env.engine(30);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    for round in 0..5 {
        let made = env
            .s
            .file(docs.join(format!("d07/just before {round}.txt")));
        let coverage = c.sync(&docs).unwrap();
        assert!(matches!(coverage.names, Proof::Live(_)), "{coverage:?}");
        assert!(
            answered(&mut c, &named(&home, &format!("just before {round}"))).contains(&made),
            "round {round}: the sync answered before the change"
        );
    }
}

/// The barrier proves the live folder alone: it runs no check of the
/// root, so a change made in a folder nothing watches waits for a sync of
/// that folder, which checks.
#[test]
fn a_sync_of_a_live_folder_checks_nothing_else() {
    let mut env = Env::new("engine-barrier-only");
    tree(&env);
    // Past the check's tick: a folder read just after it changed is read
    // again at every check, which would find what this test hides.
    std::thread::sleep(Duration::from_millis(2200));
    env.engine(30);
    let home = env.home();
    let docs = home.join("docs");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    let unwatched = env.s.file(home.join("elsewhere/unseen.txt"));
    c.sync(&docs).unwrap();
    assert!(
        !answered(&mut c, &named(&home, "unseen")).contains(&unwatched),
        "the barrier checked the root"
    );
    let coverage = c.sync(&home).unwrap();
    assert!(matches!(coverage.names, Proof::Checked(_)), "{coverage:?}");
    assert!(answered(&mut c, &named(&home, "unseen")).contains(&unwatched));
}

/// A mount made below a watched folder is a hole at once, "a mount": its
/// names are the mount's, never the folder's, and the rest stays live.
/// Taken away, the folder is read again.
#[test]
fn a_mount_made_below_a_watched_folder_is_a_hole_at_once() {
    if !in_namespace() {
        return;
    }
    let mut env = Env::new("engine-live-mount");
    tree(&env);
    env.engine(30);
    let home = env.home();
    let docs = home.join("docs");
    let target = docs.join("d03");
    let mut c = env.client("a test");
    c.watch(&[&docs], &[]).unwrap();
    let report = named(&home, "report 3.txt");
    assert_eq!(answered(&mut c, &report).len(), 1);
    env.s.mount_tmpfs(&target);
    wait_for(5, "the mount a hole", || {
        c.coverage(&docs)
            .unwrap()
            .holes
            .iter()
            .any(|h| h.path.to_path() == target && h.why == Why::Mount)
    });
    assert!(
        answered(&mut c, &report).is_empty(),
        "the folder's names let go of"
    );
    assert!(is_live(&mut c, &docs), "the rest stays live");
    assert!(
        Command::new("umount")
            .arg(&target)
            .status()
            .unwrap()
            .success()
    );
    wait_for(5, "read again once the mount is gone", || {
        answered(&mut c, &report).len() == 1
    });
    assert!(c.coverage(&docs).unwrap().holes.is_empty());
}

/// The units run the engine for the session, on the socket clients look
/// for: the user's runtime folder's `skwird/socket`, its user's alone.
#[test]
fn the_units_run_the_engine_for_the_session_on_its_socket() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/systemd");
    let service = std::fs::read_to_string(dist.join("skwird.service")).unwrap();
    let socket = std::fs::read_to_string(dist.join("skwird.socket")).unwrap();
    assert!(
        service
            .lines()
            .any(|l| l.starts_with("ExecStart=") && l.ends_with(" serve --forever")),
        "{service}"
    );
    assert!(
        service.contains("RestrictAddressFamilies=AF_UNIX"),
        "rule 6"
    );
    for line in [
        "ListenStream=%t/skwird/socket",
        "SocketMode=0600",
        "DirectoryMode=0700",
    ] {
        assert!(socket.lines().any(|l| l == line), "{line}");
    }
}

/// Started by a service manager with its socket (`LISTEN_FDS`), as
/// systemd starts skwird.service: the engine answers on the socket it was
/// passed, binds none of its own, and leaves it in place when it goes.
/// `systemd-socket-activate` stands in for systemd; nothing is installed.
#[test]
fn an_engine_answers_on_the_socket_a_service_manager_passes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if Command::new("systemd-socket-activate")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: no systemd-socket-activate");
        return;
    }
    let mut env = Env::new("engine-activated");
    tree(&env);
    let socket = env.socket();
    // The runtime folder, its user's alone, as the socket unit makes it.
    let folder = socket.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = env.home();
    let mut activate = Command::new("systemd-socket-activate");
    activate.arg("-l").arg(&socket);
    for (name, value) in [
        ("HOME", home.clone()),
        ("XDG_RUNTIME_DIR", env.s.join("run")),
        ("XDG_CACHE_HOME", env.s.join("cache")),
        ("XDG_STATE_HOME", env.s.join("state")),
        ("XDG_CONFIG_HOME", env.s.join("config")),
    ] {
        let mut set = std::ffi::OsString::from(format!("--setenv={name}="));
        set.push(value);
        activate.arg(set);
    }
    let child = activate
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_skwird"))
        .args([OsStr::new("serve"), OsStr::new("--root"), home.as_os_str()])
        .args(["--idle", "2", "--threads", "2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    env.engines.push(child);
    wait_for(5, "the socket made by the service manager", || {
        socket.exists()
    });
    let made = std::fs::metadata(&socket).unwrap().ino();
    let mut c = connect(&socket, &home, "a test");
    let coverage = c.coverage(&home).unwrap();
    assert!(matches!(coverage.names, Proof::Checked(_)), "{coverage:?}");
    assert_eq!(answered(&mut c, &everything(&home)), walked(&home));
    assert_eq!(
        std::fs::metadata(&socket).unwrap().ino(),
        made,
        "the engine bound a socket of its own"
    );
    drop(c);
    let engine = env.engines.last_mut().unwrap();
    wait_for(10, "the engine gone once idle", || {
        engine.try_wait().unwrap().is_some()
    });
    assert!(socket.exists(), "the service manager's socket was removed");
}
