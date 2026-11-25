//! How a client connects, and when it gives a connection up: an engine of
//! another version is said so, not taken for none; one that listens and
//! fails is not started again beside it; none is started for a socket it
//! would not serve on; a connection read in the middle of a line is
//! closed; on Linux, a connect to an engine whose queue is full is given
//! up on after the silence, or when stopped. Over a socket an engine of
//! the test's own listens on.

#![cfg(unix)]

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use skwird_client::protocol::{
    self, Answer, Ask, Coverage, Failure, Proof, Request, Response, Scope, Welcome, Why,
};
use skwird_client::{Client, Error, Find};

mod common;
use common::Scratch as Dir;

/// An engine of the test's own at `socket`, for one connection: each
/// request read is handed to `answer`, which writes what it says.
fn engine(socket: &Path, answer: impl Fn(Request, &mut UnixStream) + Send + 'static) {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut out = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        while let Ok(Some(request)) = protocol::read::<_, Request>(&mut reader) {
            answer(request, &mut out);
        }
    });
}

fn welcome(id: u64) -> Response {
    Response {
        id,
        answer: Answer::Hello(Welcome {
            version: protocol::VERSION,
            scope: Scope::User,
            engine: "a test's".into(),
            roots: Vec::new(),
            features: Vec::new(),
        }),
    }
}

fn outside() -> Coverage {
    Coverage {
        names: Proof::NotCovered(Why::Outside),
        attrs: Proof::NotCovered(Why::Outside),
        holes: Vec::new(),
    }
}

/// An engine that speaks no version the client speaks refuses its hello
/// so (`what` is `version`), and the client says so: the engine is not
/// taken for none.
#[test]
fn an_engine_of_another_version_is_said_so() {
    let dir = Dir::new("version");
    let socket = dir.join("socket");
    engine(&socket, |request, out| {
        let failure = Failure {
            what: "version".into(),
            message: "this engine speaks another protocol alone".into(),
        };
        let refused = Response {
            id: request.id,
            answer: Answer::Error(failure),
        };
        let _ = protocol::write(out, &refused);
    });
    let find = Find {
        socket: Some(socket),
        start: None,
        silence: Some(Duration::from_secs(5)),
        ..Find::new("a test")
    };
    let e = Client::connect(&find).err().unwrap();
    assert!(matches!(e, Error::Version), "{e}");
}

/// An engine that listens where one started would serve, and fails (here,
/// it never says hello), is not started again beside it, nor waited for:
/// one engine per scope (rule 4).
#[test]
fn an_engine_that_listens_and_fails_is_not_started_again() {
    let dir = Dir::new("listens");
    let run = dir.join("run");
    std::fs::create_dir_all(run.join("skwird")).unwrap();
    let socket = run.join("skwird/socket");
    // Bound and never accepted from: a connection is made, and nothing is
    // ever said on it.
    let _listener = UnixListener::bind(&socket).unwrap();
    let find = Find {
        socket: Some(socket),
        runtime: Some(run),
        state: Some(dir.join("state")),
        // Ends at once, serving nothing: it would be waited for in vain.
        start: Some(PathBuf::from("true")),
        wait: Duration::from_secs(10),
        silence: Some(Duration::from_millis(200)),
        ..Find::new("a test")
    };
    let begun = Instant::now();
    let e = Client::connect(&find).err().unwrap();
    let took = begun.elapsed();
    assert!(matches!(e, Error::NoEngine(_)), "{e}");
    assert!(took < Duration::from_secs(5), "waited for: {took:?}");
    // An engine started makes its log first.
    assert!(
        !dir.join("state/skwird/engine.log").exists(),
        "an engine was started"
    );
}

/// No engine is started for a socket given elsewhere than in the runtime
/// folder given: it would serve there, and the socket given would be
/// waited for in vain.
#[test]
fn no_engine_is_started_for_a_socket_it_would_not_serve_on() {
    let dir = Dir::new("elsewhere");
    let find = Find {
        socket: Some(dir.join("elsewhere/socket")),
        runtime: Some(dir.join("run")),
        state: Some(dir.join("state")),
        start: Some(PathBuf::from("true")),
        wait: Duration::from_secs(10),
        ..Find::new("a test")
    };
    let begun = Instant::now();
    let e = Client::connect(&find).err().unwrap();
    let took = begun.elapsed();
    assert!(matches!(e, Error::NoEngine(_)), "{e}");
    assert!(took < Duration::from_secs(5), "waited for: {took:?}");
    assert!(
        !dir.join("state/skwird/engine.log").exists(),
        "an engine was started"
    );
}

/// A connection given up on in the middle of a line (one longer than a
/// line may be) is closed: what follows is never read as an answer, here
/// one to the next call.
#[test]
fn a_connection_read_in_the_middle_of_a_line_is_closed() {
    let dir = Dir::new("cut");
    let socket = dir.join("socket");
    engine(&socket, |request, out| {
        use std::io::Write;
        if let Ask::Hello(_) = request.ask {
            let _ = protocol::write(out, &welcome(request.id));
            return;
        }
        // Too long a line, which ends with the answer to the next request:
        // read on from where the client gave it up, it would be one.
        let mut line = vec![b' '; protocol::MAX_LINE + 64 * 1024];
        let next = Response {
            id: request.id + 1,
            answer: Answer::Coverage(outside()),
        };
        protocol::write(&mut line, &next).unwrap();
        let _ = out.write_all(&line);
    });
    let find = Find {
        socket: Some(socket),
        start: None,
        silence: Some(Duration::from_secs(5)),
        ..Find::new("a test")
    };
    let mut c = Client::connect(&find).unwrap();
    let e = c.coverage(Path::new("/home/user")).unwrap_err();
    assert!(
        matches!(&e, Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidData),
        "{e}"
    );
    let again = c.coverage(Path::new("/home/user"));
    assert!(
        again.is_err(),
        "read on from the middle of a line: {again:?}"
    );
}

/// What `f` gives, run on a thread of its own and waited for at most
/// `wait`: none if it did not end, so that a test fails rather than hangs.
#[cfg(target_os = "linux")]
fn within<T: Send + 'static>(wait: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(wait).ok()
}

/// A connect to an engine whose queue of connections is full, which Linux
/// would hold as long as the engine accepts none, is given up on after the
/// silence, and at once when stopped.
#[cfg(target_os = "linux")]
#[test]
fn a_connect_to_a_full_queue_gives_up() {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let dir = Dir::new("full");
    let socket = dir.join("socket");
    // A queue of one, never accepted from, filled by a first connect.
    let listener = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::bind(&listener, &SocketAddrUnix::new(&socket).unwrap()).unwrap();
    rustix::net::listen(&listener, 0).unwrap();
    let _first = UnixStream::connect(&socket).unwrap();

    let find = Find {
        socket: Some(socket),
        start: None,
        silence: Some(Duration::from_millis(300)),
        ..Find::new("a test")
    };
    let silent = find.clone();
    let begun = Instant::now();
    let e = within(Duration::from_secs(10), move || {
        Client::connect(&silent).err()
    })
    .expect("the connect waited past its silence")
    .expect("a full queue connected");
    let took = begun.elapsed();
    assert!(matches!(e, Error::NoEngine(_)), "{e}");
    assert!(took >= Duration::from_millis(250), "{took:?}");

    let stopper = skwird_client::Stopper::new();
    let stopping = stopper.clone();
    let waiting = Find {
        silence: None,
        ..find
    };
    let begun = Instant::now();
    let connect = std::thread::spawn(move || Client::connect_stopped(&waiting, &stopper).err());
    std::thread::sleep(Duration::from_millis(100));
    stopping.stop();
    let e = within(Duration::from_secs(10), move || connect.join().unwrap())
        .expect("the connect waited past its stop")
        .expect("a full queue connected");
    assert!(matches!(e, Error::Stopped), "{e}");
    assert!(begun.elapsed() < Duration::from_secs(5));
}
