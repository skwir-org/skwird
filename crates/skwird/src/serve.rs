//! The engine's socket: one engine per user, held by a lock; every peer
//! checked to be its user; a connection per thread; a query on a thread
//! of its own, so that a `cancel` can reach it; the engine gone once its
//! last client has been gone a while.

use std::collections::HashMap;
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skwird_index::keeper::Event;
use skwird_index::{NameQuery, Pattern, Query};
use skwird_protocol::{self as wire, Answer, Ask, Request, Response, WirePath};

use crate::convert;
use crate::coverage::coverage_at;
use crate::dirs::{check_socket_path, make_private};
use crate::engine::{Command, Config, Published, Shared, log, writer};

/// Results sent at a time.
const BATCH: usize = 512;

/// How many clients are connected, and since when none is.
struct Live {
    state: Mutex<(usize, Instant)>,
}

impl Live {
    fn join(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0 += 1;
    }

    fn leave(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 -= 1;
        if state.0 == 0 {
            state.1 = Instant::now();
        }
    }

    /// How long until the engine may go: none while a client is here.
    fn left(&self, idle: Duration) -> Option<Duration> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.0 == 0).then(|| idle.saturating_sub(state.1.elapsed()))
    }
}

/// Serve the user's engine until it has been idle for `config.idle`, or
/// for ever. A second engine blocks on the lock until the first is gone,
/// then takes over: it never polls the lock.
pub fn serve(config: &Config) -> Result<(), String> {
    // Before any thread allocates: what a burst frees goes back.
    crate::memory::settle();
    let socket = config.dirs.socket();
    check_socket_path(&socket)?;
    make_private(&config.dirs.runtime)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.dirs.lock())
        .map_err(|e| format!("cannot open the lock: {e}"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| format!("cannot take the lock: {e}"))?;
    // The lock is ours: a socket left there is a dead engine's.
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).map_err(|e| format!("cannot bind the socket: {e}"))?;
    log(&format!("serving {} roots", config.roots.len()));
    let shared = Arc::new(Shared::new(config.roots.clone()));
    let (commands, received) = channel();
    // One watcher for every root, listened to on a thread of its own that
    // waits on it alone.
    let watcher = crate::live::watcher(config.watches);
    if let Some(w) = &watcher {
        let (w, commands) = (Arc::clone(w), commands.clone());
        std::thread::spawn(move || crate::live::listen(&w, &commands));
    }
    let writing = {
        let (config, shared) = (config.clone(), Arc::clone(&shared));
        std::thread::spawn(move || writer(&config, &shared, &received, watcher))
    };
    let live = Arc::new(Live {
        state: Mutex::new((0, Instant::now())),
    });
    let me = rustix::process::geteuid();
    loop {
        // Woken by a client, or when the idle time may be up: no polling.
        let wait = config.idle.map(|idle| live.left(idle).unwrap_or(idle));
        if wait.is_some_and(|w| w.is_zero()) {
            break;
        }
        let timeout = wait.map(|w| rustix::event::Timespec {
            tv_sec: i64::try_from(w.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(w.subsec_nanos()),
        });
        let mut fds = [rustix::event::PollFd::new(
            &listener,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(e) => return Err(format!("cannot wait for clients: {e}")),
        }
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        // A user's engine answers its user alone.
        match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(peer) if peer.uid == me => {}
            _ => continue,
        }
        live.join();
        let (shared, commands, live) = (Arc::clone(&shared), commands.clone(), Arc::clone(&live));
        std::thread::spawn(move || {
            connection(stream, &shared, &commands);
            live.leave();
        });
    }
    let _ = commands.send(Command::Stop);
    let _ = writing.join();
    let _ = std::fs::remove_file(&socket);
    log("idle: gone");
    drop(lock);
    Ok(())
}

type Writer = Arc<Mutex<UnixStream>>;

fn send(to: &Writer, id: u64, answer: Answer) -> bool {
    let mut stream = to.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    wire::write(&mut *stream, &Response { id, answer }).is_ok()
}

fn failure(what: &str, message: impl Into<String>) -> Answer {
    Answer::Error(wire::Failure {
        what: what.to_owned(),
        message: message.into(),
    })
}

fn outside() -> wire::Coverage {
    wire::Coverage {
        names: wire::Proof::NotCovered(wire::Why::Outside),
        attrs: wire::Proof::NotCovered(wire::Why::Outside),
        holes: Vec::new(),
    }
}

/// The coverage below `root`, from the snapshot published now.
fn coverage(shared: &Shared, root: &WirePath) -> Result<wire::Coverage, Answer> {
    let path = root.to_path();
    let Some((i, rel)) = shared.locate(&path) else {
        return Ok(outside());
    };
    match shared.snapshot(i) {
        Published::Opening => Ok(wire::Coverage {
            names: wire::Proof::NotCovered(wire::Why::Unread),
            attrs: wire::Proof::NotCovered(wire::Why::Unread),
            holes: Vec::new(),
        }),
        Published::Failed(why) => Err(failure("internal", why)),
        Published::Ready(index) => coverage_at(&index, &shared.roots[i], &rel)
            .ok_or_else(|| failure("not_a_folder", "no folder of the index is at that path")),
    }
}

/// The client's hello, answered: its name, or none if it speaks no
/// version of ours, or said something else first.
fn hello(reader: &mut BufReader<UnixStream>, writer: &Writer, shared: &Shared) -> Option<String> {
    match wire::read::<_, Request>(reader) {
        Ok(Some(Request {
            id,
            ask: Ask::Hello(hello),
        })) => {
            if !hello.versions.contains(&wire::VERSION) {
                send(
                    writer,
                    id,
                    failure("version", "this engine speaks protocol 0 alone"),
                );
                return None;
            }
            let welcome = wire::Welcome {
                version: wire::VERSION,
                scope: wire::Scope::User,
                engine: concat!("skwird ", env!("CARGO_PKG_VERSION")).to_owned(),
                roots: shared.roots.iter().map(|r| WirePath::of(r)).collect(),
            };
            send(writer, id, Answer::Hello(welcome)).then_some(hello.client)
        }
        Ok(Some(Request { id, .. })) => {
            send(writer, id, failure("bad_request", "hello comes first"));
            None
        }
        _ => None,
    }
}

/// One client: hello first, then its requests in turn, each query on a
/// thread of its own.
fn connection(stream: UnixStream, shared: &Arc<Shared>, commands: &Sender<Command>) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let writer: Writer = Arc::new(Mutex::new(clone));
    let mut reader = BufReader::new(stream);
    let Some(client) = hello(&mut reader, &writer, shared) else {
        return;
    };
    let cancels: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::default();
    let mut queries = Vec::new();
    loop {
        let request = match wire::read::<_, Request>(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                send(&writer, 0, failure("bad_request", e.to_string()));
                break;
            }
        };
        let id = request.id;
        let answer = match request.ask {
            Ask::Hello(_) => failure("bad_request", "hello comes once"),
            Ask::Coverage { root } => match coverage(shared, &root) {
                Ok(c) => Answer::Coverage(c),
                Err(a) => a,
            },
            Ask::Sync { root } => sync(shared, commands, &root),
            Ask::Query(query) => {
                let flag = Arc::new(AtomicBool::new(false));
                cancels
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, Arc::clone(&flag));
                let (shared, writer, cancels) = (
                    Arc::clone(shared),
                    Arc::clone(&writer),
                    Arc::clone(&cancels),
                );
                queries.push(std::thread::spawn(move || {
                    let answer = answer_query(&shared, &writer, id, &query, &flag);
                    cancels
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                    send(&writer, id, answer);
                }));
                continue;
            }
            Ask::Cancel { id: of } => {
                if let Some(flag) = cancels
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&of)
                {
                    flag.store(true, Ordering::Relaxed);
                }
                // The query answers itself: its `done` says it was
                // cancelled.
                continue;
            }
            Ask::Changes { root, since } => changes(shared, commands, &root, since),
            Ask::Exclude { add, remove } => {
                let (reply, answer) = channel();
                let to_path = |v: Vec<WirePath>| v.iter().map(WirePath::to_path).collect();
                let asked = commands.send(Command::Exclude {
                    client: client.clone(),
                    add: to_path(add),
                    remove: to_path(remove),
                    reply,
                });
                match asked.ok().and_then(|()| answer.recv().ok()) {
                    Some(folders) => Answer::Excluded {
                        folders: folders.iter().map(|p| WirePath::of(p)).collect(),
                    },
                    None => failure("internal", "the engine is going"),
                }
            }
            Ask::Watch { add, remove } => {
                let (reply, answer) = channel();
                let to_path = |v: Vec<WirePath>| v.iter().map(WirePath::to_path).collect();
                let asked = commands.send(Command::Watch {
                    client: client.clone(),
                    add: to_path(add),
                    remove: to_path(remove),
                    reply,
                });
                match asked.ok().and_then(|()| answer.recv().ok()) {
                    Some(folders) => Answer::Watching {
                        folders: folders.iter().map(|p| WirePath::of(p)).collect(),
                    },
                    None => failure("internal", "the engine is going"),
                }
            }
        };
        if !send(&writer, id, answer) {
            break;
        }
    }
    for q in queries {
        let _ = q.join();
    }
}

fn sync(shared: &Shared, commands: &Sender<Command>, root: &WirePath) -> Answer {
    let Some((i, _)) = shared.locate(&root.to_path()) else {
        return Answer::Coverage(outside());
    };
    let (reply, done) = channel();
    if commands.send(Command::Sync { root: i, reply }).is_err() || done.recv().is_err() {
        return failure("internal", "the engine is going");
    }
    match coverage(shared, root) {
        Ok(c) => Answer::Coverage(c),
        Err(a) => a,
    }
}

fn changes(shared: &Shared, commands: &Sender<Command>, root: &WirePath, since: String) -> Answer {
    let Some((i, rel)) = shared.locate(&root.to_path()) else {
        return Answer::TooOld {};
    };
    let (reply, answer) = channel();
    let asked = commands.send(Command::Changes {
        root: i,
        rel,
        since,
        reply,
    });
    let Some(answer) = asked.ok().and_then(|()| answer.recv().ok()) else {
        return failure("internal", "the engine is going");
    };
    let Ok((events, token)) = answer else {
        return Answer::TooOld {};
    };
    let base = &shared.roots[i];
    let events = events
        .iter()
        .map(|e| match e {
            Event::Entry { path, op, entry } => {
                let entry = convert::entry(&base.join(path), entry);
                match op {
                    skwird_index::log::Op::Added => wire::Event::Added(entry),
                    skwird_index::log::Op::Removed => wire::Event::Removed(entry),
                    skwird_index::log::Op::Changed => wire::Event::Changed(entry),
                }
            }
            Event::NotCovered { path, why } => wire::Event::NotCovered {
                path: WirePath::of(&base.join(path)),
                why: convert::why(*why),
            },
        })
        .collect();
    Answer::Changes(wire::Changes {
        events,
        token: token.text(),
    })
}

/// Answer a query from the snapshot published now, in batches, then
/// `done` with the coverage that held; cancelled, `done` says so.
fn answer_query(
    shared: &Shared,
    writer: &Writer,
    id: u64,
    query: &wire::Query,
    cancelled: &AtomicBool,
) -> Answer {
    let path = query.root.to_path();
    let done = |coverage, count, truncated| {
        Answer::Done(wire::Done {
            coverage,
            count,
            truncated,
            cancelled: cancelled.load(Ordering::Relaxed),
        })
    };
    let Some((i, rel)) = shared.locate(&path) else {
        return done(outside(), 0, false);
    };
    let index = match shared.snapshot(i) {
        Published::Ready(index) => index,
        Published::Opening => {
            return done(
                wire::Coverage {
                    names: wire::Proof::NotCovered(wire::Why::Unread),
                    attrs: wire::Proof::NotCovered(wire::Why::Unread),
                    holes: Vec::new(),
                },
                0,
                false,
            );
        }
        Published::Failed(why) => return failure("internal", why),
    };
    let Some(coverage) = coverage_at(&index, &shared.roots[i], &rel) else {
        return failure("not_a_folder", "no folder of the index is at that path");
    };
    let Some(folder) = index.folder_at(&rel) else {
        return failure("not_a_folder", "no folder of the index is at that path");
    };
    let asked = Query {
        name: query.name.as_ref().map(|n| NameQuery {
            pattern: match n.how {
                wire::Match::Skwir => Pattern::Skwir(n.text.clone()),
                wire::Match::Substring => Pattern::Substring(n.text.clone()),
                wire::Match::Glob => Pattern::Glob(n.text.clone()),
                wire::Match::Exact => Pattern::Exact(n.text.clone()),
            },
            case_sensitive: n.case_sensitive,
        }),
        kinds: query
            .kinds
            .as_ref()
            .map(|ks| ks.iter().map(|&k| convert::kind_of(k)).collect()),
        size: query.size.map(|[a, b]| a..=b),
        mtime: query
            .mtime
            .map(|[a, b]| convert::time_of(a)..=convert::time_of(b)),
        // One more than asked, to say whether there were more.
        limit: query
            .limit
            .map(|l| usize::try_from(l).unwrap_or(usize::MAX).saturating_add(1)),
    };
    let hits = match index.query(folder, &asked) {
        Ok(hits) => hits,
        Err(e) => return failure("bad_pattern", e),
    };
    let limit = query
        .limit
        .map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let truncated = hits.len() > limit;
    let root: &Path = &shared.roots[i];
    let mut sent = 0u64;
    for batch in hits[..hits.len().min(limit)].chunks(BATCH) {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let entries = batch.iter().map(|h| convert::hit(root, h)).collect();
        if !send(writer, id, Answer::Results(entries)) {
            break;
        }
        sent += batch.len() as u64;
    }
    done(coverage, sent, truncated)
}
