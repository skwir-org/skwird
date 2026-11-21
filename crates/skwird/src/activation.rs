//! Socket activation (`docs/PLAN.md`, phase 2, step 6): the listening
//! socket a service manager made for the engine and passed to it, as
//! systemd passes it (`LISTEN_PID`, `LISTEN_FDS`, the first descriptor
//! after the standard three). The engine then answers on it, and neither
//! binds nor removes a socket of its own.
//!
//! This module takes a descriptor the process was given, which only an
//! unsafe call can do, so it allows unsafe code (AGENTS.md, "Unsafe").

#![allow(unsafe_code)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::net::{AddressFamily, SocketType};

/// The first descriptor a service manager passes (`SD_LISTEN_FDS_START`).
const FIRST: i32 = 3;

/// Whether the passed socket was taken: it is owned once.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// The socket a service manager passed this process, if it passed one:
/// `LISTEN_PID` names this process and `LISTEN_FDS` holds one or more.
/// Refused unless it is a Unix stream socket, listening.
pub fn passed() -> Result<Option<UnixListener>, String> {
    let pid = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    if pid != Some(std::process::id()) {
        return Ok(None);
    }
    let count: i32 = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if count < 1 || TAKEN.swap(true, Ordering::SeqCst) {
        return Ok(None);
    }
    // SAFETY: the service manager passed descriptor 3 to this process,
    // open, as `LISTEN_PID` and `LISTEN_FDS` say; nothing else in the
    // process owns it, and `TAKEN` makes this the only place that takes
    // it, once.
    let fd = unsafe { OwnedFd::from_raw_fd(FIRST) };
    let what = |e: rustix::io::Errno| format!("the socket passed cannot be read: {e}");
    if rustix::net::sockopt::socket_domain(&fd).map_err(what)? != AddressFamily::UNIX
        || rustix::net::sockopt::socket_type(&fd).map_err(what)? != SocketType::STREAM
        || !rustix::net::sockopt::socket_acceptconn(&fd).map_err(what)?
    {
        return Err("the socket passed is not a Unix stream socket, listening".into());
    }
    // Passed as it was inherited: kept from whatever the engine starts.
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).map_err(what)?;
    Ok(Some(UnixListener::from(fd)))
}
