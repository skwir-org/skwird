//! What changes below each root on Windows (`docs/PLAN.md`, phase 4, step
//! 5): one `ReadDirectoryChangesW` on each root's folder, its whole tree,
//! read on a thread of its own. Unlike inotify, it takes no watch for each
//! folder: a root is heard whole, as the system service hears a marked
//! filesystem on Linux.
//!
//! - Each entry made, removed, renamed, written to or given attributes is
//!   said by its path below its root ([`Heard::Entry`]): its folder is read
//!   again, as an inotify event names an entry in a watched folder.
//! - A buffer that overflowed (`ERROR_NOTIFY_ENUM_DIR`, or nothing given)
//!   is an overflow of that root ([`Heard::Overflow`]): nothing heard there
//!   proves anything until a check.
//! - A watch that ends (its folder removed, its volume taken away) says so
//!   ([`Heard::Lost`]): the root is no longer heard.
//! - A cookie made in the folder given ([`Watcher::watch_cookies`]) is said
//!   as one ([`Heard::Cookie`]), in its turn after every change below that
//!   root before it: the sync barrier of a root holding that folder.
//!
//! The root's folder is held open while it is watched, which AGENTS.md's
//! "Hold nothing of a filesystem between reads" forbids elsewhere: opened
//! to share reading, writing and deletion, it stops no rename or removal.

use std::collections::HashMap;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use windows_sys::Win32::Foundation::{
    ERROR_NOTIFY_ENUM_DIR, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects,
};

use super::{Handle, verbatim};

mod follow;

/// What a root's watch heard.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Heard {
    /// The entry at `rel` below root `scope` was made, removed, renamed,
    /// written to or given other attributes: its folder is read again.
    Entry { scope: usize, rel: PathBuf },
    /// What root `scope`'s watch heard was dropped: nothing heard there
    /// proves anything until a check.
    Overflow { scope: usize },
    /// Root `scope`'s watch ended: it is no longer heard.
    Lost { scope: usize },
    /// The cookie of this number was made ([`Watcher::watch_cookies`]):
    /// every change below its root heard before it has been read.
    Cookie(u64),
}

/// A root watched: its thread, and the event that stops it.
struct Watched {
    stop: Arc<Handle>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watched {
    fn drop(&mut self) {
        // SAFETY: the thread's own stop event, alive while it runs.
        unsafe { SetEvent(self.stop.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the roots' threads heard, waiting to be read.
#[derive(Default)]
struct Queue {
    heard: Mutex<Vec<Heard>>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, more: impl IntoIterator<Item = Heard>) {
        let mut heard = self.heard.lock().unwrap_or_else(PoisonError::into_inner);
        let before = heard.len();
        heard.extend(more);
        if heard.len() > before {
            self.ready.notify_all();
        }
    }
}

/// The watcher of a user's engine on Windows: each root heard whole.
pub struct Watcher {
    generation: u64,
    queue: Arc<Queue>,
    roots: Mutex<HashMap<usize, Watched>>,
    /// The folder cookies are made in, as the system names it.
    cookies: Arc<Mutex<Option<PathBuf>>>,
    /// The journal reads done, every followed root's ([`Watcher::follow_root`]).
    journal_reads: Arc<AtomicU64>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Watcher {
    /// A watcher of generation `generation`. Windows needs no watch for
    /// each folder: there is no budget to keep to.
    // The same call as on Linux, which may fail.
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(generation: u64, _budget: usize) -> std::io::Result<Watcher> {
        Ok(Watcher {
            generation,
            queue: Arc::default(),
            roots: Mutex::new(HashMap::new()),
            cookies: Arc::new(Mutex::new(None)),
            journal_reads: Arc::default(),
        })
    }

    /// No share of watches to keep to on Windows.
    pub fn share() -> usize {
        0
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Hear every change below the root `scope` at `path` from now on.
    pub fn watch_root(&self, scope: usize, path: &Path) -> std::io::Result<()> {
        let name = verbatim(path);
        // SAFETY: a NUL-terminated name alive for the call, no security
        // attributes, no template; the handle given is ours to close.
        let dir = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if dir == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // The first read asked before this returns: what changes from now
        // on is heard, never lost before the thread first asks. A root
        // heard from now is read after, so that what it holds is either
        // read or heard (AGENTS.md, "Watch, then open, then list").
        let mut reading = Reading {
            dir: Handle(dir),
            done: event()?,
            // SAFETY: an all-zero structure is a valid one, before its event.
            ov: Box::new(unsafe { std::mem::zeroed() }),
            buffer: vec![0u64; 32 * 1024],
        };
        if !reading.ask() {
            return Err(std::io::Error::last_os_error());
        }
        let stop = Arc::new(event()?);
        let (queue, cookies, root) = (
            Arc::clone(&self.queue),
            Arc::clone(&self.cookies),
            path.to_path_buf(),
        );
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("skwird-heard".into())
                .spawn(move || listen(reading, &stop, scope, &root, &queue, &cookies))?
        };
        let watched = Watched {
            stop,
            thread: Some(thread),
        };
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(scope, watched);
        Ok(())
    }

    /// Hear every change below the root `scope` at `path` from now on,
    /// through its volume's change journal (the system service's way,
    /// [`follow`]): from the journal's next record, so that what is made
    /// once this returns is heard whenever it is read.
    pub fn follow_root(&self, scope: usize, path: &Path) -> std::io::Result<()> {
        let mut follower = follow::Follower::start(path)?;
        if !follower.ask() {
            return Err(std::io::Error::last_os_error());
        }
        let stop = Arc::new(event()?);
        let (queue, cookies, reads, root) = (
            Arc::clone(&self.queue),
            Arc::clone(&self.cookies),
            Arc::clone(&self.journal_reads),
            path.to_path_buf(),
        );
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("skwird-journal".into())
                .spawn(move || {
                    follow::run(follower, &stop, scope, &root, &queue, &cookies, &reads);
                })?
        };
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                scope,
                Watched {
                    stop,
                    thread: Some(thread),
                },
            );
        Ok(())
    }

    /// The journal reads done so far, every followed root's: one for each
    /// batch of records, none while nothing changes.
    pub fn journal_reads(&self) -> u64 {
        self.journal_reads.load(Ordering::Relaxed)
    }

    /// Hear nothing of root `scope` any more.
    pub fn unwatch_root(&self, scope: usize) {
        let gone = self
            .roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&scope);
        drop(gone);
    }

    /// Say a file named `cookie-N` made in `dir` as [`Heard::Cookie`]: in
    /// its turn, for the root holding `dir`.
    // The same call as on Linux, which may fail.
    #[allow(clippy::unnecessary_wraps)]
    pub fn watch_cookies(&self, dir: &Path) -> std::io::Result<()> {
        *self.cookies.lock().unwrap_or_else(PoisonError::into_inner) = Some(dir.to_path_buf());
        Ok(())
    }

    /// Wait until something was heard, then take it all.
    pub fn read(&self, out: &mut Vec<Heard>) -> std::io::Result<()> {
        let mut heard = self
            .queue
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while heard.is_empty() {
            heard = self
                .queue
                .ready
                .wait(heard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        out.append(&mut heard);
        Ok(())
    }

    // What Linux's watcher has for its watches of each folder, which
    // Windows does not take: nothing to keep, nothing to let go of.

    pub fn watching(&self) -> usize {
        0
    }

    pub fn budget(&self) -> usize {
        0
    }

    pub fn set_scopes(&self, _: Vec<PathBuf>) {}

    pub fn wants(&self, _: &Path) -> bool {
        false
    }

    pub fn unwatch(&self, _: i32) {}
}

/// An event, made unsignaled and reset by hand.
fn event() -> std::io::Result<Handle> {
    // SAFETY: no attributes, no name: a new event of this process's own.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Handle(event))
}

/// What a root's watch is asked to hear: every name made, removed or
/// renamed, and every change to what an entry is or holds.
const CHANGES: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

/// A root's folder being read for changes: the read pending, its
/// structure and its buffer at places of their own until it is done.
struct Reading {
    dir: Handle,
    done: Handle,
    ov: Box<OVERLAPPED>,
    buffer: Vec<u64>,
}

// SAFETY: moved once, to the thread that reads, and used by it alone
// after; the structure and the buffer stay at their places, boxed.
unsafe impl Send for Reading {}

impl Reading {
    /// Ask for the next changes: whether the read is pending.
    fn ask(&mut self) -> bool {
        // SAFETY: an all-zero structure is a valid one, given its event.
        *self.ov = unsafe { std::mem::zeroed() };
        self.ov.hEvent = self.done.0;
        let len = u32::try_from(self.buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: the buffer, aligned for the records, and its length; it
        // and the structure stay in place until the call is done or its
        // cancel has landed, which `listen` waits for.
        unsafe {
            ReadDirectoryChangesW(
                self.dir.0,
                self.buffer.as_mut_ptr().cast::<c_void>(),
                len,
                1,
                CHANGES,
                std::ptr::null_mut(),
                &raw mut *self.ov,
                None,
            ) != 0
        }
    }
}

/// Read what changes below the root `reading` reads, its first read
/// asked, until `stop` is set, each batch put in `queue`.
fn listen(
    mut reading: Reading,
    stop: &Handle,
    scope: usize,
    root: &Path,
    queue: &Queue,
    cookies: &Mutex<Option<PathBuf>>,
) {
    let dir = reading.dir.0;
    loop {
        let waits: [HANDLE; 2] = [stop.0, reading.done.0];
        // SAFETY: two events of ours, alive for the wait.
        let woken = unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) };
        let mut moved = 0u32;
        if woken == WAIT_OBJECT_0 {
            // SAFETY: the pending call's own structure, alive until its
            // cancel has landed, which the wait for its result waits for.
            unsafe {
                CancelIoEx(dir, &raw const *reading.ov);
                GetOverlappedResult(dir, &raw const *reading.ov, &raw mut moved, 1);
            }
            return;
        }
        // SAFETY: the call's own structure, done.
        let ok = unsafe { GetOverlappedResult(dir, &raw const *reading.ov, &raw mut moved, 1) };
        if ok == 0 {
            // SAFETY: no argument.
            if unsafe { GetLastError() } != ERROR_NOTIFY_ENUM_DIR {
                queue.push([Heard::Lost { scope }]);
                return;
            }
            queue.push([Heard::Overflow { scope }]);
        } else if moved == 0 {
            // More than the buffer held: dropped.
            queue.push([Heard::Overflow { scope }]);
        } else {
            let cookies = cookies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let bytes = &super::bytemuck(&reading.buffer)[..moved as usize];
            queue.push(records(bytes, scope, root, cookies.as_deref()));
        }
        if !reading.ask() {
            queue.push([Heard::Lost { scope }]);
            return;
        }
    }
}

/// The changes a buffer of `FILE_NOTIFY_INFORMATION` records says, each
/// below root `scope` at `root`; a cookie made in `cookies` said as one.
fn records(bytes: &[u8], scope: usize, root: &Path, cookies: Option<&Path>) -> Vec<Heard> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let u32_at = |o: usize| {
            bytes
                .get(at + o..at + o + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let (Some(next), Some(action), Some(len)) = (u32_at(0), u32_at(4), u32_at(8)) else {
            break;
        };
        let Some(name) = bytes.get(at + 12..at + 12 + len as usize) else {
            break;
        };
        let wide: Vec<u16> = name
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let rel = PathBuf::from(OsString::from_wide(&wide));
        let cookie = (action == FILE_ACTION_ADDED)
            .then(|| cookies.filter(|c| root.join(&rel).parent() == Some(c)))
            .flatten()
            .and_then(|_| {
                rel.file_name()?
                    .to_str()?
                    .strip_prefix("cookie-")?
                    .parse::<u64>()
                    .ok()
            });
        out.push(match cookie {
            Some(n) => Heard::Cookie(n),
            None => Heard::Entry { scope, rel },
        });
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(action: u32, name: &str, last: bool) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        // Each record padded to 4 bytes, as the system aligns them.
        let size = (12 + wide.len()).next_multiple_of(4);
        let mut out = Vec::new();
        out.extend(
            if last {
                0
            } else {
                u32::try_from(size).unwrap()
            }
            .to_le_bytes(),
        );
        out.extend(action.to_le_bytes());
        out.extend(u32::try_from(wide.len()).unwrap().to_le_bytes());
        out.extend(wide);
        out.resize(size, 0);
        out
    }

    /// Each record names an entry below its root; a cookie made in the
    /// cookies' folder is said as one; a cookie's name anywhere else, or
    /// removed, is an entry.
    #[test]
    fn records_say_entries_and_cookies() {
        let root = Path::new(r"C:\Users\someone");
        let cookies = root.join(r"AppData\Local\skwird\run\cookies");
        let mut bytes = record(1, r"docs\notes.txt", false);
        bytes.extend(record(
            1,
            r"AppData\Local\skwird\run\cookies\cookie-7",
            false,
        ));
        bytes.extend(record(
            2,
            r"AppData\Local\skwird\run\cookies\cookie-6",
            false,
        ));
        bytes.extend(record(1, r"docs\cookie-8", true));
        let heard = records(&bytes, 3, root, Some(&cookies));
        assert_eq!(
            heard,
            vec![
                Heard::Entry {
                    scope: 3,
                    rel: PathBuf::from(r"docs\notes.txt")
                },
                Heard::Cookie(7),
                Heard::Entry {
                    scope: 3,
                    rel: PathBuf::from(r"AppData\Local\skwird\run\cookies\cookie-6")
                },
                Heard::Entry {
                    scope: 3,
                    rel: PathBuf::from(r"docs\cookie-8")
                },
            ]
        );
        // Cut short: what is whole is said, nothing past the buffer read.
        let cut = &bytes[..bytes.len() - 6];
        assert_eq!(records(cut, 3, root, Some(&cookies)).len(), 3);
    }
}

#[cfg(test)]
mod watched {
    use std::time::Duration;

    use skwird_testkit::Scratch;

    use super::*;

    /// What the watcher hears, read on one thread of its own for as long
    /// as the receiver is kept: each batch sent as it comes.
    fn hearing(w: &Arc<Watcher>) -> std::sync::mpsc::Receiver<Vec<Heard>> {
        let (send, got) = std::sync::mpsc::channel();
        let w = Arc::clone(w);
        std::thread::spawn(move || {
            loop {
                let mut heard = Vec::new();
                if w.read(&mut heard).is_err() || send.send(heard).is_err() {
                    return;
                }
            }
        });
        got
    }

    /// Everything heard until `done` holds of it, up to 5 s.
    fn heard_until(w: &Arc<Watcher>, done: impl Fn(&[Heard]) -> bool) -> Vec<Heard> {
        let got = hearing(w);
        let mut all = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(&all) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "{all:?}");
            if let Ok(heard) = got.recv_timeout(left) {
                all.extend(heard);
            }
        }
        all
    }
    /// A root's tree is heard: an entry made deep in it is named by its
    /// path below the root.
    #[test]
    fn an_entry_made_below_a_root_is_heard_by_its_path() {
        let s = Scratch::new("watch-root");
        s.dir(r"root\a\b");
        let w = Arc::new(Watcher::new(1, 0).unwrap());
        w.watch_root(4, &s.join("root")).unwrap();
        // Wine watches a folder of the tree, on its host's inotify, a while
        // after the first read is asked, and only once the folder was opened
        // (as the engine's crawl opens each, right after the watch).
        // Windows records every change in the tree from the first read on,
        // which `watch_root` asks before it returns.
        if skwird_testkit::under_wine() {
            std::thread::sleep(Duration::from_millis(500));
            for rel in [r"root\a", r"root\a\b"] {
                let _ = std::fs::read_dir(s.join(rel)).map(Iterator::count);
            }
        }
        s.file(r"root\a\b\made.txt");
        heard_until(&w, |all| {
            all.iter().any(
                |h| matches!(h, Heard::Entry { scope: 4, rel } if rel == Path::new(r"a\b\made.txt")),
            )
        });
    }

    /// A root whose folder is removed says its watch ended: it is heard no
    /// more, and its root is checked until it is watched again.
    #[test]
    fn a_root_removed_says_its_watch_ended() {
        let s = Scratch::new("watch-lost");
        s.file(r"root\a\file");
        let w = Arc::new(Watcher::new(1, 0).unwrap());
        w.watch_root(2, &s.join("root")).unwrap();
        s.remove("root");
        // Wine keeps a removed folder's watch, hearing its entries go and
        // nothing after: the end is proven on Windows alone.
        if skwird_testkit::under_wine() {
            eprintln!("not proven under Wine: a removed root's watch ends");
            return;
        }
        heard_until(&w, |all| all.contains(&Heard::Lost { scope: 2 }));
    }
}
