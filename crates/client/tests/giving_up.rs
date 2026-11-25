//! Every call can be given up on (skwir's provider, step B): after the
//! engine's silence, or from another thread. An engine that accepts and
//! then hangs, or one slow to check, never holds a client's search. On
//! Unix over a socket, on Windows over a named pipe.

#![cfg(any(unix, windows))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use skwird_client::fake::{Fake, Script};
use skwird_client::protocol::{Query, WirePath};
use skwird_client::{Client, Error, Find, Stopper};

mod common;

/// A folder of the test's own (`common::Scratch`); on Windows, a pipe's
/// name of its own.
#[cfg(unix)]
struct Dir(common::Scratch);

#[cfg(windows)]
struct Dir(PathBuf);

impl Dir {
    #[cfg(unix)]
    fn new(label: &str) -> Dir {
        Dir(common::Scratch::new(label))
    }

    #[cfg(windows)]
    fn new(label: &str) -> Dir {
        Dir(format!(r"\\.\pipe\skwird-test-{label}-{}", std::process::id()).into())
    }

    /// Its folder; on Windows, its pipe's name.
    fn path(&self) -> &Path {
        #[cfg(unix)]
        return &self.0.0;
        #[cfg(windows)]
        &self.0
    }

    /// Where an engine of the test's answers.
    fn socket(&self) -> PathBuf {
        if cfg!(windows) {
            self.path().to_path_buf()
        } else {
            self.path().join("socket")
        }
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
        links: skwird_client::protocol::Links::Own,
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

/// `stopper` stopped from another thread after a little while.
fn stop_soon(stopper: &Stopper) -> std::thread::JoinHandle<()> {
    let stopper = stopper.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        stopper.stop();
    })
}

/// A connect whose engine never says hello, waited on as long as it
/// takes, is stopped from another thread at once; so is every connect
/// given the same stopper after.
#[test]
fn a_connect_stopped_at_its_hello_returns_at_once() {
    let dir = Dir::new("stop-hello");
    let socket = dir.socket();
    #[cfg(unix)]
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    #[cfg(windows)]
    let _listener = skwird_client::pipe::Server::bind(socket.as_os_str()).unwrap();
    let stopper = Stopper::new();
    let stopping = stop_soon(&stopper);
    let begun = Instant::now();
    let e = Client::connect_stopped(&find(&socket, None), &stopper)
        .err()
        .unwrap();
    assert!(matches!(e, Error::Stopped), "{e}");
    assert!(begun.elapsed() < Duration::from_secs(5));
    stopping.join().unwrap();
    let begun = Instant::now();
    let e = Client::connect_stopped(&find(&socket, None), &stopper)
        .err()
        .unwrap();
    assert!(matches!(e, Error::Stopped), "{e}");
    assert!(begun.elapsed() < Duration::from_millis(100));
}

/// A connect stopped before it starts an engine starts none: a program
/// that is not there is never tried.
#[test]
fn a_connect_stopped_before_it_starts_no_engine() {
    let dir = Dir::new("stop-start");
    let stopper = Stopper::new();
    stopper.stop();
    let find = Find {
        start: Some(PathBuf::from("no such program of skwird's")),
        ..find(&dir.socket(), None)
    };
    let e = Client::connect_stopped(&find, &stopper).err().unwrap();
    assert!(matches!(e, Error::Stopped), "{e}");
}

/// A started engine waited for (one that never serves) is given up on at
/// once when stopped, not when its wait ends.
#[cfg(unix)]
#[test]
fn a_started_engine_waited_for_is_stopped_at_once() {
    let dir = Dir::new("stop-wait");
    let find = Find {
        // Ends at once, serving nothing.
        start: Some(PathBuf::from("true")),
        wait: Duration::from_secs(30),
        runtime: Some(dir.path().to_path_buf()),
        state: Some(dir.path().to_path_buf()),
        // Where an engine started with that runtime folder would serve.
        ..find(&dir.path().join("skwird/socket"), None)
    };
    let stopper = Stopper::new();
    let stopping = stop_soon(&stopper);
    let begun = Instant::now();
    let e = Client::connect_stopped(&find, &stopper).err().unwrap();
    assert!(matches!(e, Error::Stopped), "{e}");
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );
    stopping.join().unwrap();
}

/// The stopper a connection was made with stops its calls, as the one
/// taken from it does: they are the same.
#[test]
fn a_connection_is_stopped_by_the_stopper_it_was_made_with() {
    let dir = Dir::new("stop-made");
    let fake = silent(&dir);
    let stopper = Stopper::new();
    let mut c = Client::connect_stopped(&find(&fake.socket, None), &stopper).unwrap();
    let stopping = stop_soon(&stopper);
    let begun = Instant::now();
    let e = c.query(&everything(), |_| true).unwrap_err();
    assert!(matches!(e, Error::Stopped), "{e}");
    assert!(begun.elapsed() < Duration::from_secs(5));
    stopping.join().unwrap();
    let mut again = Client::connect(&find(&fake.socket, None)).unwrap();
    let taken = again.stopper().unwrap();
    let stopping = stop_soon(&taken);
    let e = again.coverage(Path::new("/home/user")).unwrap_err();
    assert!(matches!(e, Error::Stopped), "{e}");
    stopping.join().unwrap();
}
