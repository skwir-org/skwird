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

/// A sync of a root holding the cookies' folder is a barrier: it answers
/// live, with a change made just before it, every event before its
/// cookie read. A sync of a root that does not hold it checks, as no
/// cookie can be heard in its turn there, and never waits for one.
#[test]
fn a_sync_is_a_barrier_where_the_cookies_are_heard() {
    let mut env = Env::new("win-barrier").local_in_home();
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    wait_for(10, "live", || is_live(&mut c, &home));
    let made = env.s.file(home.join(r"docs\d07\just before.txt"));
    let synced = c.sync(&home).unwrap();
    assert!(matches!(synced.names, Proof::Live(_)), "{synced:?}");
    assert!(answered(&mut c, &named(&home, "just before")).contains(&made));
    drop(c);
    drop(env);

    let mut env = Env::new("win-checked");
    tree(&env);
    env.engine(30);
    let mut c = env.client();
    let home = env.home();
    wait_for(10, "live", || is_live(&mut c, &home));
    let made = env.s.file(home.join(r"docs\d07\just before.txt"));
    let begun = Instant::now();
    let synced = c.sync(&home).unwrap();
    assert!(begun.elapsed() < Duration::from_secs(5), "it waited");
    // Checked, then live as it was.
    assert!(
        matches!(synced.names, Proof::Live(_) | Proof::Checked(_)),
        "{synced:?}"
    );
    assert!(answered(&mut c, &named(&home, "just before")).contains(&made));
}

/// skwirdw, skwird with no console window, serves as skwird does, and
/// logs to the engine's log in its state folder, having no console.
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

/// The system's pipe (`skwird_client::pipe::Server::bind_system`): its
/// owner, as an administrator makes it, is the administrators, and a
/// client takes it so; a user who is not one may connect to it, but not
/// make an instance of it, which would answer the service's clients; an
/// administrator may. On a machine of the tests' own alone, which may make
/// a user.
#[test]
#[allow(unsafe_code)] // A pipe's instance made as another user has no safe binding; each block says why it is sound.
fn the_systems_pipe_is_every_users_to_reach_and_none_to_answer() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE};
    if !skwird_testkit::throwaway() {
        eprintln!("skipped: makes a user (SKWIRD_TEST_MACHINE)");
        return;
    }
    let name = format!(r"\\.\pipe\skwird-test-system-{}", std::process::id());
    let server = skwird_client::pipe::Server::bind_system(name.as_ref()).unwrap();
    let accepting = std::thread::spawn(move || {
        let stream = server.accept(Some(Duration::from_secs(10))).unwrap();
        (server, stream)
    });
    let client = skwird_client::pipe::Stream::connect(name.as_ref(), Some(Duration::from_secs(5)));
    assert!(client.is_ok(), "taken as the system's: {:?}", client.err());
    let (_server, accepted) = accepting.join().unwrap();
    assert!(accepted.is_some());
    // An instance made, as `ConnectNamedPipe`'s server makes one.
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let instance = || {
        // SAFETY: a NUL-terminated name; no security attributes: the
        // pipe's own descriptor decides.
        let pipe = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE,
                255,
                512,
                512,
                0,
                std::ptr::null(),
            )
        };
        let error = std::io::Error::last_os_error();
        if pipe != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            // SAFETY: the instance made, ours, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(pipe) };
            return Ok(());
        }
        Err(error)
    };
    assert!(instance().is_ok(), "an administrator makes an instance");
    let user = skwird_testkit::User::make("skwird-pipe", "Sk-1wird.pipe!");
    let token = user.logon();
    let denied = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                // SAFETY: a primary token of ours, the thread's alone while
                // it makes its instance, then the thread's own again.
                assert_ne!(unsafe { ImpersonateLoggedOnUser(token.as_raw_handle()) }, 0);
                let made = instance();
                // SAFETY: no argument.
                assert_ne!(unsafe { RevertToSelf() }, 0);
                made
            })
            .join()
            .unwrap()
    });
    assert_eq!(
        denied.map_err(|e| e.raw_os_error()),
        Err(Some(5)),
        "a user makes no instance of it"
    );
}

/// A pipe another user made is refused, whatever its name and however
/// open it is: a user of the tests' own serves a pipe every user may read
/// and write, as one squatting the service's name would, and this user's
/// client refuses it before saying anything, its owner and its server's
/// user neither the system's nor its own. On a machine of the tests' own
/// alone.
#[test]
fn a_pipe_another_user_made_is_refused() {
    if !skwird_testkit::throwaway() {
        eprintln!("skipped: makes a user (SKWIRD_TEST_MACHINE)");
        return;
    }
    let s = Scratch::new("win-squat");
    let user = skwird_testkit::User::make("skwird-squat", "Sk-1wird.squat!");
    let folder = s.dir("squatter");
    skwird_testkit::grant(&folder, user.name);
    let name = format!("skwird-test-squat-{}", std::process::id());
    let script = s.write(
        r"squatter\squat.ps1",
        format!(
            "$security = New-Object System.IO.Pipes.PipeSecurity\n\
             $everyone = New-Object System.Security.Principal.SecurityIdentifier('S-1-1-0')\n\
             $security.AddAccessRule((New-Object System.IO.Pipes.PipeAccessRule($everyone, 'ReadWrite', 'Allow')))\n\
             $pipe = New-Object System.IO.Pipes.NamedPipeServerStream('{name}', 'InOut', 1, 'Byte', 'None', 4096, 4096, $security)\n\
             $pipe.WaitForConnection()\n\
             Start-Sleep -Seconds 30\n"
        )
        .as_bytes(),
    );
    let system =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let _squatter = user.spawn(&format!(
        "\"{}\" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        system
            .join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
            .display(),
        script.display()
    ));
    let pipe = format!(r"\\.\pipe\{name}");
    let deadline = Instant::now() + Duration::from_secs(60);
    let refused = loop {
        match skwird_client::pipe::Stream::connect(pipe.as_ref(), Some(Duration::from_secs(1))) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            said => break said,
        }
    };
    let Err(e) = refused else {
        panic!("another user's pipe was taken");
    };
    // Refused by the client's check, not by the pipe's own access.
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
    assert!(e.raw_os_error().is_none(), "{e}");
    assert!(e.to_string().contains("another user's"), "{e}");
}
