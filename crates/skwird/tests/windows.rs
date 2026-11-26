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
use skwird_protocol::{Links, Match, NameTerm, Proof, Query, Why, WirePath};
use skwird_testkit::{Scratch, reference_walk};

/// The program. Built from Linux for Windows, cargo names it by the
/// host's path; under Wine, the host's root is drive `Z:`.
fn program() -> PathBuf {
    windows_path(env!("CARGO_BIN_EXE_skwird"))
}

fn windows_path(built: &str) -> PathBuf {
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
    /// The application data folder: outside the home, or in it, as
    /// Windows has it ([`Env::local_in_home`]).
    local: PathBuf,
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
        let local = s.join("local");
        Env {
            s,
            pipe,
            local,
            engines: Vec::new(),
        }
    }

    /// The application data folder in the home, as Windows has it: the
    /// engine's folders, its cookies' among them, below its root.
    fn local_in_home(mut self) -> Env {
        self.local = self.s.dir(r"home\AppData\Local");
        self
    }

    fn home(&self) -> PathBuf {
        self.s.join("home")
    }

    /// The program, with every folder it may reach this test's own.
    fn command(&self, args: &[&str]) -> Command {
        self.command_of(program(), args)
    }

    /// `program`, with every folder it may reach this test's own.
    fn command_of(&self, program: PathBuf, args: &[&str]) -> Command {
        let mut c = Command::new(program);
        c.args(args)
            .env("USERPROFILE", self.home())
            .env("LOCALAPPDATA", &self.local)
            .env("SKWIRD_PIPE", &self.pipe)
            // An engine a client of the test starts goes soon after.
            .env("SKWIRD_IDLE", "1");
        c
    }

    /// An engine of the home, gone after `idle` seconds without a client,
    /// its log in the test's folder ([`Env::logs`]).
    fn engine(&mut self, idle: u64) -> usize {
        let home = self.home();
        let idle = idle.to_string();
        let log = self.s.join(format!("engine-{}.log", self.engines.len()));
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
            .env("SKWIRD_LOG", &log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.engines.push(child);
        self.engines.len() - 1
    }

    /// What each engine this test started logged, for a failure to say.
    fn logs(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for n in 0..self.engines.len() {
            let log = std::fs::read_to_string(self.s.join(format!("engine-{n}.log")));
            let _ = writeln!(out, "--- engine {n}:\n{}", log.unwrap_or_default());
        }
        out
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
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let said = Client::connect(&self.find()).map(|mut c| c.coverage(&home));
            if said.as_ref().is_ok_and(|cov| {
                cov.as_ref()
                    .is_ok_and(|c| c.names != Proof::NotCovered(Why::Unread))
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the engine never answered (last: {}):\n{}",
                match &said {
                    Ok(Ok(c)) => format!("{:?}", c.names),
                    Ok(Err(e)) | Err(e) => e.to_string(),
                },
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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
    // Heard whole once read (phase 4, step 5), or checked until then.
    let coverage = c.coverage(&home).unwrap();
    assert!(
        matches!(coverage.names, Proof::Live(_) | Proof::Checked(_)),
        "{coverage:?}"
    );
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
    // Checked at a time after it was asked; or live, a check having
    // applied everything made before it (no cookie is heard in this root:
    // the test's application data is outside it).
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

fn is_live(c: &mut Client, path: &Path) -> bool {
    matches!(c.coverage(path).unwrap().names, Proof::Live(_))
}

/// Each root is heard whole (phase 4, step 5): live with no folder
/// registered, and a change made in it, deep or new, is answered without
/// a sync, within the gathering's second.
#[test]
fn a_change_is_answered_live_without_a_sync() {
    let mut env = Env::new("win-live");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    wait_for(10, "live", || is_live(&mut c, &home));
    assert!(is_live(&mut c, &home.join(r"docs\d05")));
    let made = env.s.file(home.join(r"docs\d05\made live.txt"));
    let deep = env.s.file(home.join(r"new\folder\deep inside.txt"));
    wait_for(5, "the changes answered without a sync", || {
        let found = answered(&mut c, &named(&home, "live.txt"));
        found.contains(&made) && answered(&mut c, &named(&home, "deep inside")).contains(&deep)
    });
    // Removed, it is gone from the answers as soon.
    env.s.remove(r"home\docs\d05\made live.txt");
    wait_for(5, "a removal answered without a sync", || {
        answered(&mut c, &named(&home, "made live")).is_empty()
    });
    assert!(is_live(&mut c, &home));
}

/// A sync of any root is a barrier: its cookie, asked of the root's own
/// thread, is said after a change made just before it, and the sync
/// answers live with the change, at once, whether the root holds the
/// engine's folders or not. A cookie made as a file was heard by the root
/// holding the cookies' folder alone, and a sync of any other root checked
/// it whole. Under Wine, which tells changes late, a sync checks, and
/// answers the change all the same.
#[test]
fn a_sync_of_any_root_is_a_barrier() {
    let wine = skwird_testkit::under_wine();
    for (name, holds_its_folders) in [("win-barrier", true), ("win-barrier-apart", false)] {
        let mut env = Env::new(name);
        if holds_its_folders {
            env = env.local_in_home();
        }
        tree(&env);
        env.engine(30);
        let mut c = env.client();
        let home = env.home();
        wait_for(10, "live", || is_live(&mut c, &home));
        let mut took = Vec::new();
        for i in 0..10 {
            let made = env
                .s
                .file(home.join(format!(r"docs\d07\just before {i}.txt")));
            let begun = Instant::now();
            let synced = c.sync(&home).unwrap();
            took.push(begun.elapsed());
            if wine {
                // Checked, then live as it was.
                assert!(
                    matches!(synced.names, Proof::Live(_) | Proof::Checked(_)),
                    "{name}: {synced:?}"
                );
            } else {
                assert!(matches!(synced.names, Proof::Live(_)), "{name}: {synced:?}");
            }
            assert!(
                answered(&mut c, &named(&home, &format!("just before {i}"))).contains(&made),
                "{name}, sync {i}"
            );
        }
        took.sort();
        // Well before it would give up on its cookie (2 s) and check.
        assert!(
            took[took.len() - 1] < Duration::from_millis(1500),
            "{name}: {took:?}"
        );
        if !wine {
            assert!(
                took[took.len() / 2] < Duration::from_millis(200),
                "{name}: {took:?}"
            );
        }
    }
}

/// skwirdw, skwird with no console window, serves as skwird does, and
/// logs to the engine's log in its state folder, having no console.
/// `skwird stop` from another copy of the program, in a folder of its
/// own, leaves the engine running and says so; from this copy's, it stops
/// it, and gives back once its process ended, whose program's file is
/// then free; with none running, it says so. Each exits 0: this copy's
/// engine runs no more. Run as an installer runs it, `skwirdw.exe stop`
/// makes no folder of skwird's.
#[test]
fn stop_ends_this_copys_engine_alone() {
    let mut env = Env::new("win-stop");
    tree(&env);
    let skwirdw = windows_path(env!("CARGO_BIN_EXE_skwirdw"));
    assert_eq!(stop_by(&env, skwirdw), "no engine of yours answers");
    assert!(!env.local.join("skwird").exists(), "a folder of skwird's");
    let engine = env.engine(600);
    drop(env.client());
    let other = env.s.join(r"elsewhere\skwird.exe");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::copy(program(), &other).unwrap();
    let said = stop_by(&env, other.clone());
    assert!(
        said.contains("another copy of skwird, left running"),
        "{said}"
    );
    assert!(
        env.engines[engine].try_wait().unwrap().is_none(),
        "it stayed"
    );
    drop(env.client());
    assert_eq!(stop_by(&env, program()), "your engine stopped");
    assert!(
        env.engines[engine].try_wait().unwrap().is_some(),
        "its process ended first:\n{}",
        env.logs()
    );
    assert_eq!(stop_by(&env, other), "no engine of yours answers");
}

/// What `skwird stop` said, run from the program at `program`, which
/// exited 0.
fn stop_by(env: &Env, program: PathBuf) -> String {
    let out = env.command_of(program, &["stop"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(
        out.status.success(),
        "{said} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    said
}

#[test]
fn skwirdw_serves_and_logs_with_no_console() {
    let env = Env::new("win-skwirdw");
    tree(&env);
    let home = env.home();
    let mut child = env
        .command_of(
            windows_path(env!("CARGO_BIN_EXE_skwirdw")),
            &["serve", "--root", home.to_str().unwrap(), "--idle", "30"],
        )
        .spawn()
        .unwrap();
    let mut c = env.client();
    assert!(!answered(&mut c, &named(&home, "notes")).is_empty());
    drop(c);
    let log = env.local.join(r"skwird\state\engine.log");
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(said.contains("serving 1 roots"), "{said}");
    let _ = child.kill();
    let _ = child.wait();
}

/// `skwird logon add` writes the task's definition, in UTF-16, its
/// program skwirdw.exe and its user the one running it, and has
/// `schtasks` register it; `remove` takes it back. Wine's `schtasks` keeps
/// no task, so what is registered is proven on Windows, where this test
/// registers one for the user running it: on a machine of the tests' own
/// alone (`skwird_testkit::throwaway`), and under Wine, in the tests' own
/// prefix. Skipped on any other Windows.
#[test]
fn the_logon_task_is_written_and_registered() {
    let throwaway = skwird_testkit::throwaway();
    if !skwird_testkit::under_wine() && !throwaway {
        eprintln!("skipped: it would register a task of the user running it (SKWIRD_TEST_MACHINE)");
        return;
    }
    let env = Env::new("win-logon");
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
    let bytes = std::fs::read(env.local.join(r"skwird\state\logon.xml")).unwrap();
    assert_eq!(
        &bytes[..2],
        &[0xFF, 0xFE],
        "UTF-16, its byte order mark first"
    );
    let units: Vec<u16> = bytes[2..]
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let xml = String::from_utf16(&units).unwrap();
    let sid = skwird_client::pipe::user_sid().unwrap();
    assert_eq!(xml.matches(&format!("<UserId>{sid}</UserId>")).count(), 2);
    assert!(xml.contains(r#"skwirdw.exe"</Command>"#), "{xml}");
    assert!(xml.contains("<Arguments>serve --forever</Arguments>"));
    // Registered, where a system keeps tasks: run as its user, at logon.
    let task = skwird::logon::name(&sid);
    let query = || {
        std::process::Command::new("schtasks")
            .args(["/Query", "/TN", &task, "/XML"])
            .output()
            .unwrap()
    };
    if throwaway {
        let out = query();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let kept = String::from_utf8_lossy(&out.stdout);
        assert!(kept.contains("<LogonTrigger>"), "{kept}");
        assert!(kept.contains("skwirdw.exe"), "{kept}");
    }
    run("remove");
    if throwaway {
        assert!(!query().status.success(), "the task is taken back");
    }
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

/// `diskpart` run on `script`'s lines.
fn diskpart(env: &Env, name: &str, script: &[String]) -> bool {
    let file = env.s.join(format!("{name}.diskpart"));
    std::fs::write(&file, script.join("\r\n")).unwrap();
    Command::new("diskpart")
        .arg("/s")
        .arg(&file)
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the volume mounted at `at` can be locked: only where no handle
/// but this one is open on it (`FSCTL_LOCK_VOLUME`); unlocked at once.
#[allow(unsafe_code)] // Windows' volume lock has no safe binding; each block says why it is sound.
fn locks(at: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeNameForVolumeMountPointW,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME};
    let mut point: Vec<u16> = at.as_os_str().encode_wide().collect();
    point.extend("\\\0".encode_utf16());
    let mut name = [0u16; 64];
    // SAFETY: a NUL-ended path, and a buffer of its length.
    let named =
        unsafe { GetVolumeNameForVolumeMountPointW(point.as_ptr(), name.as_mut_ptr(), 64) } != 0;
    assert!(
        named,
        "no volume mounted at the disk's folder: {}",
        std::io::Error::last_os_error()
    );
    let end = name.iter().position(|&c| c == 0).unwrap_or(0);
    // `\\?\Volume{…}`, its trailing backslash left out: the volume, not
    // its root folder.
    name[end - 1] = 0;
    // SAFETY: a NUL-ended name; a handle of ours, closed below.
    let volume = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if volume == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut got = 0u32;
    // SAFETY: the handle above; no buffers.
    let locked = unsafe {
        DeviceIoControl(
            volume,
            FSCTL_LOCK_VOLUME,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut got,
            std::ptr::null_mut(),
        )
    } != 0;
    // SAFETY: as above; then the handle closed once.
    unsafe {
        if locked {
            DeviceIoControl(
                volume,
                FSCTL_UNLOCK_VOLUME,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &raw mut got,
                std::ptr::null_mut(),
            );
        }
        CloseHandle(volume);
    }
    locked
}

/// The removal of the volume mounted at `at` asked, as the taskbar's
/// "Safely remove" asks it, of its volume's device
/// (`CM_Query_And_Remove_SubTreeW`): whether it went, or who vetoed it. A
/// handle open on the volume vetoes it.
#[allow(unsafe_code)] // Windows' devices have no safe binding; each block says why it is sound.
fn ask_removal(at: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
        CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
        CM_Locate_DevNodeW, CM_Query_And_Remove_SubTreeW, CM_REMOVE_UI_NOT_OK, CR_SUCCESS,
    };
    use windows_sys::Win32::Devices::Properties::DEVPKEY_Device_InstanceId;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_VOLUME;
    let text = |w: &[u16]| {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    };
    let name_of = |point: &[u16]| {
        let mut name = [0u16; 64];
        // SAFETY: a NUL-ended path, and a buffer of its length.
        let ok =
            unsafe { GetVolumeNameForVolumeMountPointW(point.as_ptr(), name.as_mut_ptr(), 64) };
        (ok != 0).then(|| text(&name))
    };
    let mut point: Vec<u16> = at.as_os_str().encode_wide().collect();
    point.extend("\\\0".encode_utf16());
    let target = name_of(&point).ok_or_else(|| {
        format!(
            "no volume mounted there: {}",
            std::io::Error::last_os_error()
        )
    })?;
    let mut len = 0u32;
    // SAFETY: the count written; the volumes' interface class; no device.
    unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            &GUID_DEVINTERFACE_VOLUME,
            std::ptr::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    let mut list = vec![0u16; len as usize];
    // SAFETY: a buffer of `len` wide characters, which the call fills.
    let listed = unsafe {
        CM_Get_Device_Interface_ListW(
            &GUID_DEVINTERFACE_VOLUME,
            std::ptr::null(),
            list.as_mut_ptr(),
            len,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if listed != CR_SUCCESS {
        return Err(format!(
            "the volumes' interfaces cannot be listed: {listed}"
        ));
    }
    for interface in list.split(|&c| c == 0).take_while(|i| !i.is_empty()) {
        let mut path = interface.to_vec();
        path.extend("\\\0".encode_utf16());
        if name_of(&path).as_deref() != Some(target.as_str()) {
            continue;
        }
        let mut named = interface.to_vec();
        named.push(0);
        let mut kind = 0u32;
        let mut id = [0u16; 512];
        let mut size = 1024u32;
        // SAFETY: a NUL-ended interface; the key; a buffer of `size` bytes.
        let got = unsafe {
            CM_Get_Device_Interface_PropertyW(
                named.as_ptr(),
                &DEVPKEY_Device_InstanceId,
                &raw mut kind,
                id.as_mut_ptr().cast(),
                &raw mut size,
                0,
            )
        };
        if got != CR_SUCCESS {
            return Err(format!("the volume's device is not told: {got}"));
        }
        let mut device = 0u32;
        // SAFETY: a NUL-ended instance id, the node written.
        let found =
            unsafe { CM_Locate_DevNodeW(&raw mut device, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
        if found != CR_SUCCESS {
            return Err(format!("the volume's device is not found: {found}"));
        }
        let mut veto = 0;
        let mut by = [0u16; 260];
        // SAFETY: the node found; the veto's kind and name written, of the
        // name's length.
        let done = unsafe {
            CM_Query_And_Remove_SubTreeW(
                device,
                &raw mut veto,
                by.as_mut_ptr(),
                260,
                CM_REMOVE_UI_NOT_OK,
            )
        };
        return if done == CR_SUCCESS {
            Ok(())
        } else {
            Err(format!("vetoed ({done}), of kind {veto}, by {}", text(&by)))
        };
    }
    Err("no device of the volume".into())
}

/// A FAT32 virtual disk of a test's, mounted at a folder (`diskpart`).
struct VDisk {
    file: PathBuf,
    at: PathBuf,
}

impl VDisk {
    /// Made, attached, formatted `TESTSTICK` and mounted at `at`.
    fn make(env: &Env, at: &Path) -> VDisk {
        let disk = VDisk {
            file: env.s.join("stick.vhdx"),
            at: at.to_path_buf(),
        };
        let made = diskpart(
            env,
            "make",
            &[
                format!(
                    "create vdisk file=\"{}\" maximum=64 type=expandable",
                    disk.file.display()
                ),
                disk.select(),
                "attach vdisk".into(),
                "create partition primary".into(),
                "format fs=fat32 quick label=TESTSTICK".into(),
                disk.mount(),
            ],
        );
        assert!(made, "diskpart: made");
        disk
    }

    fn select(&self) -> String {
        format!("select vdisk file=\"{}\"", self.file.display())
    }

    fn mount(&self) -> String {
        format!("assign mount=\"{}\"", self.at.display())
    }

    /// Attached again, and mounted at its folder.
    fn attach(&self, env: &Env) -> bool {
        diskpart(
            env,
            "attach",
            &[
                self.select(),
                "attach vdisk".into(),
                "select partition 1".into(),
                self.mount(),
            ],
        )
    }

    /// Detached, its folder's mount point taken away too: attached again,
    /// the disk may come back under another volume's name, and the folder
    /// would name the one gone.
    fn detach(&self, env: &Env) -> bool {
        let detached = diskpart(env, "detach", &[self.select(), "detach vdisk".into()]);
        let mut point = self.at.as_os_str().to_owned();
        point.push("\\");
        let _ = Command::new("mountvol")
            .arg(&point)
            .arg("/D")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        detached
    }
}

/// A removable volume a client keeps (DESIGN, Protocol, "Removable
/// volumes"), on Windows: a FAT32 virtual disk attached and mounted at a
/// folder, seen by its serial with its label, not indexed until kept.
/// Kept, a root there, its names answered, and a handle of the engine's
/// open on it (its watch's): it cannot be locked. Released by where it is
/// mounted, nothing of it is held: it can be. Detached, it is away;
/// attached again, a root again. Its removal asked as the taskbar asks it,
/// the engine lets go of it, and it goes: away. Forgotten. On a machine of
/// the tests'
/// own alone (`skwird_testkit::throwaway`): it makes a disk, as an
/// administrator.
#[test]
fn a_kept_virtual_disk_is_a_root_while_it_is_attached() {
    if !skwird_testkit::throwaway() {
        eprintln!("skipped: it makes a disk (SKWIRD_TEST_MACHINE)");
        return;
    }
    let mut env = Env::new("win-volumes");
    tree(&env);
    let at = env.s.dir("stick");
    let disk = VDisk::make(&env, &at);
    let attach = |env: &Env| disk.attach(env);
    let detach = |env: &Env| disk.detach(env);
    std::fs::write(at.join("on the stick.txt"), b"x").unwrap();
    env.engine(60);
    let mut c = env.client();
    assert!(
        c.welcome()
            .features
            .iter()
            .any(|f| f == skwird_protocol::VOLUMES)
    );
    let v = c.volumes(&[], &[], &[]).unwrap();
    let seen = v
        .seen
        .iter()
        .find(|s| s.at == WirePath::of(&at))
        .unwrap_or_else(|| panic!("the disk is not seen: {v:?}"));
    assert_eq!(seen.filesystem, "FAT32");
    assert_eq!(seen.label.as_deref(), Some("TESTSTICK"));
    assert!(!seen.kept);
    let id = seen.id.clone().expect("a serial");
    assert!(id.len() == 9 && id.as_bytes()[4] == b'-', "{id}");
    let v = c.volumes(&[&id], &[], &[]).unwrap();
    assert_eq!(
        v.kept[0].standing,
        skwird_protocol::Standing::Indexed,
        "{v:?}"
    );
    let file = at.join("on the stick.txt");
    wait_for(30, "the kept disk indexed", || {
        answered(&mut c, &named(&at, "on the stick")) == [file.clone()].into()
    });
    assert!(!locks(&at), "locked while the engine watches it");
    let v = c.release_at(&[&at]).unwrap();
    assert_eq!(v.kept[0].standing, skwird_protocol::Standing::Released);
    assert!(locks(&at), "the engine holds something on it");
    assert!(detach(&env), "diskpart: detached");
    wait_for(15, "the disk away", || {
        c.volumes(&[], &[], &[])
            .is_ok_and(|v| v.kept[0].standing == skwird_protocol::Standing::Away)
    });
    assert!(attach(&env), "diskpart: attached again");
    wait_for(30, "the disk indexed again", || {
        answered(&mut c, &named(&at, "on the stick")) == [file.clone()].into()
    });
    // An eject asked as the taskbar asks it, while the engine watches the
    // volume: it lets go of it when told, and the removal goes on.
    assert!(!locks(&at), "not watched again");
    if let Err(e) = ask_removal(&at) {
        panic!("{e}; the engine says: {:?}", c.volumes(&[], &[], &[]));
    }
    wait_for(15, "the disk away once removed", || {
        c.volumes(&[], &[], &[])
            .is_ok_and(|v| v.kept[0].standing == skwird_protocol::Standing::Away)
    });
    let v = c.volumes(&[], &[&id], &[]).unwrap();
    assert!(v.kept.is_empty(), "{v:?}");
    drop(c);
    assert!(detach(&env), "diskpart: detached");
}
