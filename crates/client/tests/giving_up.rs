//! Every call can be given up on (skwir's provider, step B): after the
//! engine's silence, or from another thread. An engine that accepts and
//! then hangs, or one slow to check, never holds a client's search. On
//! Unix over a socket, on Windows over a named pipe.

#![cfg(any(unix, windows))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use skwird_client::fake::{Fake, Script};
use skwird_client::protocol::{Query, WirePath};
use skwird_client::{Client, Error, Find};

/// A folder of the test's own under the user's cache, never `/tmp`
/// (AGENTS.md, "Testing"), removed when dropped; on Windows, a pipe's
/// name of its own.
struct Dir(PathBuf);

impl Dir {
    #[cfg(unix)]
    fn new(label: &str) -> Dir {
        let dir = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap()
            .join("skwird-tests")
            .join(format!("client-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    #[cfg(windows)]
    fn new(label: &str) -> Dir {
        Dir(format!(r"\\.\pipe\skwird-test-{label}-{}", std::process::id()).into())
    }

    /// Where an engine of the test's answers.
    fn socket(&self) -> PathBuf {
        if cfg!(windows) {
            self.0.clone()
        } else {
            self.0.join("socket")
        }
    }
}

#[cfg(unix)]
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fake engine that says hello, then nothing.
fn silent(dir: &Dir) -> Fake {
    let script = Script {
        silent: true,
        ..Script::default()
    };
    Fake::start(dir.socket(), script).unwrap()
}

fn find(socket: &Path, silence: Option<Duration>) -> Find {
    Find {
        socket: Some(socket.to_owned()),
        start: None,
        silence,
        ..Find::new("a test")
    }
}

fn everything() -> Query {
    Query {
        root: WirePath(b"/home/user".to_vec()),
        name: None,
        kinds: None,
        size: None,
        mtime: None,
        limit: None,
    }
}

/// A call the engine never answers gives up after the silence given, and
/// the connection is closed: the next call gives up at once.
#[test]
fn a_call_never_answered_gives_up_after_its_silence() {
    let dir = Dir::new("silence");
    let fake = silent(&dir);
    let silence = Duration::from_millis(200);
    let mut c = Client::connect(&find(&fake.socket, Some(silence))).unwrap();
    let begun = Instant::now();
    let e = c.coverage(Path::new("/home/user")).unwrap_err();
    let took = begun.elapsed();
    assert!(matches!(e, Error::Silent(d) if d == silence), "{e}");
    assert!(
        took >= Duration::from_millis(150) && took < Duration::from_secs(5),
        "{took:?}"
    );
    let begun = Instant::now();
    let e = c.query(&everything(), |_| true).unwrap_err();
    assert!(matches!(e, Error::Silent(_)), "{e}");
    assert!(begun.elapsed() < Duration::from_millis(100));
}

/// An engine that accepts and never says hello is given up on, and the
/// client does without it.
#[test]
fn an_engine_that_never_says_hello_is_given_up_on() {
    let dir = Dir::new("hello");
    let socket = dir.socket();
    // Bound and never accepted from: a connection is made, and nothing is
    // ever said on it.
    #[cfg(unix)]
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    #[cfg(windows)]
    let _listener = skwird_client::pipe::Server::bind(socket.as_os_str()).unwrap();
    let begun = Instant::now();
    let e = Client::connect(&find(&socket, Some(Duration::from_millis(200))))
        .err()
        .unwrap();
    assert!(matches!(e, Error::NoEngine(_)), "{e}");
    assert!(begun.elapsed() < Duration::from_secs(5));
}

/// A call stopped from another thread returns at once, as does every
/// call after it, whatever the engine does.
#[test]
fn a_call_stopped_from_another_thread_returns_at_once() {
    let dir = Dir::new("stop");
    let fake = silent(&dir);
    let mut c = Client::connect(&find(&fake.socket, None)).unwrap();
    let stopper = c.stopper().unwrap();
    let stopping = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        stopper.stop();
    });
    let begun = Instant::now();
    let e = c.query(&everything(), |_| true).unwrap_err();
    assert!(matches!(e, Error::Stopped), "{e}");
    assert!(begun.elapsed() < Duration::from_secs(5));
    stopping.join().unwrap();
    let e = c.coverage(Path::new("/home/user")).unwrap_err();
    assert!(matches!(e, Error::Stopped), "{e}");
}
