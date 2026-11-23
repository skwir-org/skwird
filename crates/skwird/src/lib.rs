//! skwird, the engine: an index of the names below its roots, kept
//! current, answered over a local socket (`docs/DESIGN.md`): a user's
//! engine, started by its first client or by the user's session, and the
//! system service. On Linux, and on Windows from phase 4 (a named pipe,
//! no watcher yet); macOS comes with phase 5.
//!
//! The library is the engine whole, so that it is tested without a
//! socket where a test needs none; the program (`main.rs`) serves it, and
//! is the command line.

#[cfg(target_os = "linux")]
pub mod activation;
#[cfg(any(target_os = "linux", windows))]
pub mod asker;
#[cfg(any(target_os = "linux", windows))]
pub mod convert;
#[cfg(any(target_os = "linux", windows))]
pub mod coverage;
pub mod dirs;
#[cfg(any(target_os = "linux", windows))]
pub mod engine;
#[cfg(any(target_os = "linux", windows))]
pub mod live;
#[cfg(any(target_os = "linux", windows))]
pub mod memory;
#[cfg(any(target_os = "linux", windows))]
pub mod os;
#[cfg(any(target_os = "linux", windows))]
pub mod serve;
pub mod system;
#[cfg(any(target_os = "linux", windows))]
pub mod transport;

/// Serve a user's engine: Linux's and Windows' alone so far (rule 7).
#[cfg(not(any(target_os = "linux", windows)))]
pub fn serve(_: &[std::path::PathBuf], _: Option<std::time::Duration>) -> Result<(), String> {
    Err("skwird's engine runs on Linux and Windows so far: macOS comes with phase 5".into())
}
