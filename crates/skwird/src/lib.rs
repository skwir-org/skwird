//! skwird, the engine: an index of the names below its roots, kept
//! current, answered over a local socket (`docs/DESIGN.md`): a user's
//! engine, started by its first client or by the user's session, and the
//! system service. On Linux, on Windows from phase 4 (a named pipe, a
//! service the service manager runs), and on macOS from phase 5.
//!
//! The library is the engine whole, so that it is tested without a
//! socket where a test needs none; the program (`main.rs`) serves it, and
//! is the command line.

#[cfg(target_os = "linux")]
pub mod activation;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod asker;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod convert;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod coverage;
pub mod daemon;
pub mod dirs;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod engine;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod live;
pub mod logon;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod memory;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod os;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod serve;
#[cfg(windows)]
pub mod service;
pub mod system;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod totals;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod transport;
pub mod volumes;

/// Serve a user's engine: Linux's, macOS' and Windows' alone (rule 7).
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn serve(_: &[std::path::PathBuf], _: Option<std::time::Duration>) -> Result<(), String> {
    Err("skwird's engine runs on Linux, macOS and Windows alone".into())
}
