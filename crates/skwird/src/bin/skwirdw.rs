//! skwirdw: skwird with no console window, for Windows (`docs/PLAN.md`,
//! phase 4, step 6). The task that starts the user's engine at logon runs
//! it (`dist/windows/skwird-logon.xml`): a console program started so
//! opens a window, which a program of Windows' GUI subsystem does not. Its
//! command line is skwird's; with no console, it logs to the engine's log
//! in its state folder, or where `SKWIRD_LOG` says. Only to serve: an
//! installer runs `skwirdw.exe stop` and `logon remove --if-mine`, unseen,
//! and those leave no folder of skwird's behind (its exit code says how
//! they went).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "../main.rs"]
mod cli;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|command| command == "serve")
        && std::env::var_os("SKWIRD_LOG").is_none()
        && let Ok(dirs) = skwird::dirs::Dirs::from_env()
        && skwird::dirs::make_private(&dirs.state).is_ok()
    {
        let _ = skwird::os::log_to(&dirs.state.join("engine.log"));
    }
    cli::main()
}

/// Windows' alone: elsewhere, skwird has no console to be rid of.
#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("skwirdw is skwird with no console window, for Windows: run skwird");
    std::process::ExitCode::FAILURE
}
