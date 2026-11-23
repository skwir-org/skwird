//! Where the engine answers its clients (`docs/DESIGN.md`, Protocol): a
//! Unix socket on Linux, its user's named pipe on Windows
//! (`skwird_client::pipe`, phase 4, step 4). The engine reads and writes
//! a [`Stream`] alike over either.

use std::path::Path;
use std::time::Duration;

/// A client's connection.
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Stream = skwird_client::pipe::Stream;

/// Where clients connect.
pub struct Listener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    inner: skwird_client::pipe::Server,
}

impl Listener {
    /// The socket a service manager made and passed (`LISTEN_FDS`).
    #[cfg(unix)]
    pub fn passed(inner: std::os::unix::net::UnixListener) -> Listener {
        Listener { inner }
    }

    /// Bound at the socket `at`.
    #[cfg(unix)]
    pub fn bind(at: &Path) -> std::io::Result<Listener> {
        Ok(Listener {
            inner: std::os::unix::net::UnixListener::bind(at)?,
        })
    }

    /// Serving the pipe `at` names, its user's alone: refused if another
    /// process holds the name.
    #[cfg(windows)]
    pub fn bind(at: &Path) -> std::io::Result<Listener> {
        Ok(Listener {
            inner: skwird_client::pipe::Server::bind(at.as_os_str())?,
        })
    }

    /// Serving the system's pipe `at` names: every user may connect, and
    /// its owner is the service's account, which no other user can make
    /// it. Refused if another process holds the name.
    #[cfg(windows)]
    pub fn bind_system(at: &Path) -> std::io::Result<Listener> {
        Ok(Listener {
            inner: skwird_client::pipe::Server::bind_system(at.as_os_str())?,
        })
    }

    /// The next client, waited for at most `wait`, as long as it takes if
    /// none: none once it passed, or where a client went before it was
    /// taken. Woken by a client alone: no polling.
    #[cfg(unix)]
    pub fn accept(&self, wait: Option<Duration>) -> std::io::Result<Option<Stream>> {
        let timeout = wait.map(|w| rustix::event::Timespec {
            tv_sec: i64::try_from(w.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(w.subsec_nanos()),
        });
        let mut fds = [rustix::event::PollFd::new(
            &self.inner,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(0) | Err(rustix::io::Errno::INTR) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(self.inner.accept().ok().map(|(stream, _)| stream))
    }

    /// The next client, waited for at most `wait`.
    #[cfg(windows)]
    pub fn accept(&self, wait: Option<Duration>) -> std::io::Result<Option<Stream>> {
        self.inner.accept(wait)
    }
}
