//! An engine a client starts (skwir's provider, step B): it runs from
//! `/`, so it holds no folder of the client's busy; it is given the
//! folders the client names; and it is reaped once it ends, no zombie of
//! the client's. A program of the test's own stands for the engine: it
//! says what it was started with, and ends.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use skwird_client::{Client, Error, Find};

/// Wait until `done` holds, up to 5 s.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_engine_started_runs_from_the_root_with_the_folders_given_and_is_reaped() {
    // Under the user's cache, never `/tmp` (AGENTS.md, "Testing").
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap()
        .join("skwird-tests")
        .join(format!("client-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let program = dir.join("bin/engine");
    std::fs::write(
        &program,
        "#!/bin/sh\n\
         here=$(dirname \"$0\")\n\
         { echo $$; pwd; echo \"$XDG_RUNTIME_DIR\"; echo \"$XDG_STATE_HOME\"; echo \"$1\"; } \
         > \"$here/said.tmp\"\n\
         mv \"$here/said.tmp\" \"$here/said\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (run, state) = (dir.join("run"), dir.join("state"));
    let find = Find {
        start: Some(program),
        // Its socket alone: no other engine is asked.
        socket: Some(run.join("skwird/socket")),
        runtime: Some(run.clone()),
        state: Some(state.clone()),
        wait: Duration::from_millis(300),
        ..Find::new("a test")
    };
    let e = Client::connect(&find).err().unwrap();
    assert!(matches!(e, Error::NoEngine(_)), "{e}");

    let said = dir.join("bin/said");
    wait_for("the program never ran", || said.exists());
    let said = std::fs::read_to_string(said).unwrap();
    let lines: Vec<&str> = said.lines().collect();
    let [pid, cwd, runtime, state_home, arg] = lines[..] else {
        panic!("{said}")
    };
    assert_eq!(cwd, "/", "it runs from the root");
    assert_eq!(Path::new(runtime), run);
    assert_eq!(Path::new(state_home), state);
    assert_eq!(arg, "serve");
    assert!(state.join("skwird/engine.log").exists(), "its log");
    // Reaped: gone, not a zombie of this process's.
    let proc = PathBuf::from(format!("/proc/{pid}"));
    wait_for("the program ended a zombie of the client's", || {
        !proc.exists()
    });
    let _ = std::fs::remove_dir_all(&dir);
}
