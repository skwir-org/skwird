//! The engine's side of a named pipe on Windows ([`skwird_client::pipe`]):
//! its name always has an instance waiting, a client accepted or not.

#![cfg(windows)]

use std::time::Duration;

use skwird_client::pipe::{Server, Stream};

/// A client that connects between two accepts finds an instance waiting,
/// the one before it accepted and gone: never no pipe, which would have it
/// start an engine in place of the one there, and leave the name for
/// another process to make its own.
#[test]
fn a_pipe_always_has_an_instance_waiting() {
    let name = std::ffi::OsString::from(format!(
        r"\\.\pipe\skwird-test-instance-{}",
        std::process::id()
    ));
    let server = Server::bind(&name).unwrap();
    let wait = Some(Duration::from_secs(5));
    let first = Stream::connect(&name, wait).unwrap();
    let accepted = server.accept(wait).unwrap().expect("the first client");
    drop(accepted);
    drop(first);
    // Between two accepts, with no connection open.
    let second = Stream::connect(&name, wait);
    assert!(second.is_ok(), "no instance waited: {:?}", second.err());
    assert!(server.accept(wait).unwrap().is_some(), "the second client");
}
