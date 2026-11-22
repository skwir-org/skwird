//! skwird, the engine: an index of the names below its roots, kept
//! current, answered over a local socket (`docs/DESIGN.md`). Phase 1: a
//! user's engine on Linux, started by its first client, gone a minute
//! after its last.
//!
//! The library is the engine whole, so that it is tested without a
//! socket where a test needs none; the program (`main.rs`) serves it, and
//! is the command line.

#[cfg(target_os = "linux")]
pub mod activation;
#[cfg(target_os = "linux")]
pub mod convert;
#[cfg(target_os = "linux")]
pub mod coverage;
pub mod dirs;
#[cfg(target_os = "linux")]
pub mod engine;
#[cfg(target_os = "linux")]
pub mod live;
#[cfg(target_os = "linux")]
pub mod memory;
#[cfg(target_os = "linux")]
pub mod serve;
pub mod system;

/// Serve a user's engine: Linux's alone so far (rule 7).
#[cfg(not(target_os = "linux"))]
pub fn serve(_: &[std::path::PathBuf], _: Option<std::time::Duration>) -> Result<(), String> {
    Err(
        "skwird's engine runs on Linux so far: Windows comes with phase 4, macOS with phase 5"
            .into(),
    )
}
