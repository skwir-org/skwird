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
//!   ([`Heard::Lost`]): the root is no longer heard. So does a root whose
//!   folder is no longer where it was ([`Place`]): renamed or moved, its
//!   handle still tells what changes below it, but its path leads to none
//!   of it. Its own rename is told in its parent alone, whose folders'
//!   names are heard too; a folder above renamed, at the next change below
//!   it.
//! - A name may be told by its short form (`LONGFO~1`), the index knowing
//!   its long one alone: each that may be one is found, and said by its
//!   long name ([`long_names`]).
//! - A cookie asked of a root ([`Watcher::cookie`]) is said by the root's
//!   own thread ([`Heard::Cookie`]), in its turn after every change below
//!   it before it: the sync barrier of every root, with no file made. NTFS
//!   tells a change to the read pending as the change is made, before the
//!   call that made it returns, and the read is done before the thread
//!   sees a cookie asked (it waits for its read ahead of its cookies). A
//!   cookie made as a file proved a single root, the one holding the
//!   cookies' folder: a sync of any other root checked it whole, 0.9 s for
//!   the runners' tree of 69,333 folders. Wine tells a change later, its
//!   server reading the host's inotify: no cookie is asked there
//!   ([`told_in_turn`]), and a sync checks.
//!
//! The root's folder, and its parent's, are held open while it is watched,
//! which AGENTS.md's "Hold nothing of a filesystem between reads" forbids
//! elsewhere: opened to share reading, writing and deletion, they stop no
//! rename or removal.

use std::collections::HashMap;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use windows_sys::Win32::Foundation::{
    ERROR_NOTIFY_ENUM_DIR, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
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
    /// The cookie of this number, asked of its root ([`Watcher::cookie`]):
    /// every change below that root before it has been read.
    Cookie(u64),
    /// Root `scope`'s journal was read up to `at`: every change before it
    /// has been heard, and once applied, the next start may resume from
    /// there (DESIGN 2). Said as a root is followed, and after each batch.
    Position {
        scope: usize,
        at: super::journal::Position,
    },
    /// Root `scope`, followed from a position kept, has read its journal
    /// up to where it was as it started: what changed while no engine ran
    /// has been heard, the end of a replay (DESIGN 2).
    Replayed { scope: usize },
}

/// A root watched: its thread, the event that stops it, and the cookies
/// asked of it.
struct Watched {
    stop: Arc<Handle>,
    asked: Arc<Asked>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The cookies asked of a root's thread and not said yet, and the event
/// that tells it so.
struct Asked {
    event: Handle,
    numbers: Mutex<Vec<u64>>,
}

impl Asked {
    fn new() -> std::io::Result<Asked> {
        Ok(Asked {
            event: event()?,
            numbers: Mutex::new(Vec::new()),
        })
    }

    fn ask(&self, n: u64) {
        let mut numbers = self.numbers.lock().unwrap_or_else(PoisonError::into_inner);
        numbers.push(n);
        // SAFETY: an event of ours, alive while `self` is.
        unsafe { SetEvent(self.event.0) };
    }

    /// The cookies asked since the last time, the event reset with them.
    fn take(&self) -> Vec<u64> {
        let mut numbers = self.numbers.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: an event of ours, alive while `self` is.
        unsafe { ResetEvent(self.event.0) };
        std::mem::take(&mut *numbers)
    }
}

/// Whether the system tells a change to a read pending as it is made,
/// before the call that made it returns: Windows does, Wine does not (its
/// server reads the host's inotify, a while after), and a cookie asked of
/// a root's thread proves nothing there.
pub fn told_in_turn() -> bool {
    static TOLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TOLD.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        // SAFETY: a NUL-terminated name; ntdll is loaded in every process.
        let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
        // SAFETY: a NUL-terminated name, of a module loaded; nothing is
        // called.
        module.is_null()
            || unsafe { GetProcAddress(module, c"wine_get_version".as_ptr().cast()) }.is_none()
    })
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
        let stop = Arc::new(event()?);
        let asked = Arc::new(Asked::new()?);
        let mut root = Reading::open(path, true, CHANGES, 32 * 1024)?;
        // Its parent's folders' names, where it has a parent: the root's
        // own folder renamed, moved or removed is told there alone. One
        // that cannot be heard so is proven where it is at each change
        // below it.
        let mut parent = path
            .parent()
            .and_then(|p| Reading::open(p, false, FILE_NOTIFY_CHANGE_DIR_NAME, 512).ok());
        let place = Place::of(&root.dir);
        // The first read asked before this returns: what changes from now
        // on is heard, never lost before the thread first asks. A root
        // heard from now is read after, so that what it holds is either
        // read or heard (AGENTS.md, "Watch, then open, then list").
        if !root.ask() {
            return Err(std::io::Error::last_os_error());
        }
        if parent.as_mut().is_some_and(|p| !p.ask()) {
            parent = None;
        }
        let listening = Listening {
            root,
            parent,
            place,
            path: path.to_path_buf(),
        };
        let queue = Arc::clone(&self.queue);
        // A read pending when this fails is cancelled, and waited for, as
        // what reads is dropped.
        let thread = {
            let (stop, asked) = (Arc::clone(&stop), Arc::clone(&asked));
            std::thread::Builder::new()
                .name("skwird-heard".into())
                .spawn(move || listen(listening, &stop, &asked, scope, &queue))?
        };
        let watched = Watched {
            stop,
            asked,
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
        self.follow_root_from(scope, path, None).map(|_| ())
    }

    /// [`Watcher::follow_root`], from the position `kept` where the journal
    /// still holds it (DESIGN 2): what changed since is heard first, then
    /// [`Heard::Replayed`]. Whether it resumed so; otherwise it is followed
    /// from the journal's next record. Its position is said at once
    /// ([`Heard::Position`]), and after each batch.
    pub fn follow_root_from(
        &self,
        scope: usize,
        path: &Path,
        kept: Option<super::journal::Position>,
    ) -> std::io::Result<bool> {
        // A read pending when this fails is cancelled, and waited for, as
        // the follower is dropped.
        let mut follower = follow::Follower::start(path, kept)?;
        if !follower.ask() {
            return Err(std::io::Error::last_os_error());
        }
        let (resumed, replaying) = follower.resumed();
        let mut said = vec![Heard::Position {
            scope,
            at: follower.position(),
        }];
        if resumed && !replaying {
            // Nothing was written since the position kept.
            said.push(Heard::Replayed { scope });
        }
        self.queue.push(said);
        let stop = Arc::new(event()?);
        let asked = Arc::new(Asked::new()?);
        let (queue, reads, root) = (
            Arc::clone(&self.queue),
            Arc::clone(&self.journal_reads),
            path.to_path_buf(),
        );
        let thread = {
            let (stop, asked) = (Arc::clone(&stop), Arc::clone(&asked));
            std::thread::Builder::new()
                .name("skwird-journal".into())
                .spawn(move || {
                    follow::run(follower, &stop, &asked, scope, &root, &queue, &reads);
                })?
        };
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                scope,
                Watched {
                    stop,
                    asked,
                    thread: Some(thread),
                },
            );
        Ok(resumed)
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

    /// Ask root `scope`'s thread for the cookie `n`, said
    /// ([`Heard::Cookie`]) once every change below the root before it is
    /// read: whether it was asked. Not where the root is not heard, nor
    /// where the system tells changes late ([`told_in_turn`]).
    pub fn cookie(&self, scope: usize, n: u64) -> bool {
        if !told_in_turn() {
            return false;
        }
        let roots = self.roots.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(watched) = roots.get(&scope) else {
            return false;
        };
        watched.asked.ask(n);
        true
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

/// A folder being read for changes, a root's tree or its parent's own
/// names: the read pending, its structure and its buffer at places of
/// their own until it is done. Dropped with a read pending, the read is
/// cancelled and waited for first, so that nothing is written where they
/// were.
struct Reading {
    dir: Handle,
    done: Handle,
    ov: Box<OVERLAPPED>,
    buffer: Vec<u64>,
    /// Its whole tree (a root's), or its own entries alone (a parent's).
    subtree: bool,
    changes: u32,
    pending: bool,
}

// SAFETY: moved once, to the thread that reads, and used by it alone
// after; the structure and the buffer stay at their places, boxed.
unsafe impl Send for Reading {}

impl Reading {
    /// The folder at `path`, opened to be read for `changes`, its tree's
    /// or its own, into a buffer of `words` words. Opened to share
    /// reading, writing and deletion: it stops no rename or removal.
    fn open(path: &Path, subtree: bool, changes: u32, words: usize) -> std::io::Result<Reading> {
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
        let dir = Handle(dir);
        Ok(Reading {
            dir,
            done: event()?,
            // SAFETY: an all-zero structure is a valid one, before its event.
            ov: Box::new(unsafe { std::mem::zeroed() }),
            buffer: vec![0u64; words],
            subtree,
            changes,
            pending: false,
        })
    }

    /// Ask for the next changes: whether the read is pending.
    fn ask(&mut self) -> bool {
        // SAFETY: an all-zero structure is a valid one, given its event.
        *self.ov = unsafe { std::mem::zeroed() };
        self.ov.hEvent = self.done.0;
        let len = u32::try_from(self.buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: the buffer, aligned for the records, and its length; it
        // and the structure stay in place until the call is done or its
        // cancel has landed, which `result` and the drop wait for.
        self.pending = unsafe {
            ReadDirectoryChangesW(
                self.dir.0,
                self.buffer.as_mut_ptr().cast::<c_void>(),
                len,
                i32::from(self.subtree),
                self.changes,
                std::ptr::null_mut(),
                &raw mut *self.ov,
                None,
            ) != 0
        };
        self.pending
    }

    /// The read done, its event set: the bytes it gave, or its error.
    fn result(&mut self) -> Result<&[u8], u32> {
        let mut moved = 0u32;
        // SAFETY: the call's own structure, done.
        let ok = unsafe { GetOverlappedResult(self.dir.0, &raw const *self.ov, &raw mut moved, 1) };
        self.pending = false;
        if ok == 0 {
            // SAFETY: no argument.
            return Err(unsafe { GetLastError() });
        }
        Ok(&super::bytemuck(&self.buffer)[..moved as usize])
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        if self.pending {
            let mut moved = 0u32;
            // SAFETY: the pending call's own structure, alive until its
            // cancel has landed, which the wait for its result waits for;
            // the handle closed after, with the fields.
            unsafe {
                CancelIoEx(self.dir.0, &raw const *self.ov);
                GetOverlappedResult(self.dir.0, &raw const *self.ov, &raw mut moved, 1);
            }
        }
    }
}

/// Where a root's folder was as it began to be heard, as its handle
/// names it. A folder renamed or moved keeps its handle, and every change
/// below it is still told there, by its path below it; but the root's
/// path no longer leads to it, nor to anything heard. None where its path
/// cannot be told (a volume mounted at a folder alone, `VOLUME_NAME_DOS`
/// has no name for), which proves nothing.
pub(super) struct Place(Option<PathBuf>);

impl Place {
    pub(super) fn of(handle: &Handle) -> Place {
        Place(final_dos(handle).ok())
    }

    /// Whether the folder `handle` opened is no longer where it was: it,
    /// or a folder above it, renamed, moved or removed; or its path can no
    /// longer be told.
    pub(super) fn moved(&self, handle: &Handle) -> bool {
        self.0
            .as_ref()
            .is_some_and(|at| final_dos(handle).ok().as_ref() != Some(at))
    }
}

/// The path of what `handle` opened, as clients name it (`C:\Users`):
/// its long names, links resolved, its volume by its letter.
pub(super) fn final_dos(handle: &Handle) -> Result<PathBuf, u32> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: the buffer and its length in units; the count returned
        // says how much was written, or how much is needed.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                handle.0,
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if written == 0 {
            // SAFETY: no argument.
            return Err(unsafe { GetLastError() });
        }
        if written < buffer.len() {
            buffer.truncate(written);
            return Ok(super::plain(Path::new(&OsString::from_wide(&buffer))));
        }
        buffer.resize(written + 1, 0);
    }
}

/// What a root's thread reads: the root's tree, its parent's names where
/// they can be heard, where the root was as it began, and its path.
struct Listening {
    root: Reading,
    parent: Option<Reading>,
    place: Place,
    path: PathBuf,
}

/// Read what changes below the root `l.root` reads, its first read asked,
/// until `stop` is set, each batch put in `queue`; each cookie `asked`
/// said once what came before it is. A root whose folder is no longer
/// where it was (its parent's names changed, or a change below it read)
/// is lost: its path leads to nothing heard.
fn listen(mut l: Listening, stop: &Handle, asked: &Asked, scope: usize, queue: &Queue) {
    loop {
        // The read before the cookies: a wait gives the first of those
        // set, so a change told before a cookie was asked is read first.
        let mut waits: Vec<HANDLE> = vec![stop.0, l.root.done.0, asked.event.0];
        if let Some(p) = &l.parent {
            waits.push(p.done.0);
        }
        // SAFETY: three or four events of ours, alive for the wait.
        let woken = unsafe {
            WaitForMultipleObjects(
                u32::try_from(waits.len()).unwrap_or(3),
                waits.as_ptr(),
                0,
                INFINITE,
            )
        };
        if woken == WAIT_OBJECT_0 {
            // What is pending is cancelled as it is dropped.
            return;
        }
        if woken == WAIT_OBJECT_0 + 2 {
            // Every change told before this was read: the read pending was
            // given none.
            queue.push(asked.take().into_iter().map(Heard::Cookie));
            continue;
        }
        if woken == WAIT_OBJECT_0 + 3
            && let Some(parent) = &mut l.parent
        {
            // A folder beside the root made, renamed or removed (or more of
            // them than the buffer held): the root's own, maybe.
            let heard = !matches!(parent.result(), Err(e) if e != ERROR_NOTIFY_ENUM_DIR);
            if l.place.moved(&l.root.dir) {
                queue.push([Heard::Lost { scope }]);
                return;
            }
            // Its parent no longer heard (gone, or its volume): the root's
            // place is proven at each change below it alone.
            let kept = heard && parent.ask();
            if !kept {
                l.parent = None;
            }
            continue;
        }
        let said = match l.root.result() {
            // More than the buffer held (nothing given) is dropped: an
            // overflow, as one the system says.
            Ok([]) | Err(ERROR_NOTIFY_ENUM_DIR) => vec![Heard::Overflow { scope }],
            Ok(bytes) => records(bytes, scope),
            Err(_) => {
                queue.push([Heard::Lost { scope }]);
                return;
            }
        };
        if l.place.moved(&l.root.dir) {
            queue.push([Heard::Lost { scope }]);
            return;
        }
        queue.push(said.into_iter().map(|h| match h {
            Heard::Entry { scope, rel } => Heard::Entry {
                scope,
                rel: long_names(&l.path, &rel),
            },
            other => other,
        }));
        if !l.root.ask() {
            queue.push([Heard::Lost { scope }]);
            return;
        }
    }
}

/// `rel`, below the root at `root`, each of its names that may be a short
/// one (`LONGFO~1`: a tilde, twelve characters at most) said by its long
/// name. A change may be told by the short name it was made by, and the
/// index knows its entries by their long names alone: a folder heard so
/// would be found nowhere, a file heard so not read again. A name that
/// cannot be found (gone since) ends the path there: its folder is read
/// again, the name gone from it, and what was below it with it.
fn long_names(root: &Path, rel: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in rel.components() {
        let std::path::Component::Normal(name) = part else {
            out.push(part);
            continue;
        };
        let wide: Vec<u16> = name.encode_wide().collect();
        if wide.len() > 12 || !wide.contains(&u16::from(b'~')) {
            out.push(name);
            continue;
        }
        let Some(long) = long_name(&root.join(&out).join(name)) else {
            out.push(name);
            return out;
        };
        out.push(long);
    }
    out
}

/// The long name of the entry at `path`, found by its name, short or
/// long: none if nothing is there.
fn long_name(path: &Path) -> Option<OsString> {
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, WIN32_FIND_DATAW,
    };
    let name = verbatim(path);
    // SAFETY: an all-zero structure is a valid one, for the call to fill.
    let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated name with no wildcard (a name holds none),
    // the structure written; the search is ours to close.
    let find = unsafe {
        FindFirstFileExW(
            name.as_ptr(),
            FindExInfoBasic,
            (&raw mut found).cast::<c_void>(),
            FindExSearchNameMatch,
            std::ptr::null(),
            0,
        )
    };
    if find == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: the search made above, closed once.
    unsafe { FindClose(find) };
    let end = found
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(found.cFileName.len());
    Some(OsString::from_wide(&found.cFileName[..end]))
}

/// The changes a buffer of `FILE_NOTIFY_INFORMATION` records says, each
/// below root `scope`. A buffer cut short, or a record past its end, is
/// an overflow after what was whole: what it held is not known.
fn records(bytes: &[u8], scope: usize) -> Vec<Heard> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let u32_at = |o: usize| {
            bytes
                .get(at + o..at + o + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let (Some(next), Some(len)) = (u32_at(0), u32_at(8)) else {
            out.push(Heard::Overflow { scope });
            break;
        };
        let Some(name) = bytes.get(at + 12..at + 12 + len as usize) else {
            out.push(Heard::Overflow { scope });
            break;
        };
        let wide: Vec<u16> = name
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let rel = PathBuf::from(OsString::from_wide(&wide));
        out.push(Heard::Entry { scope, rel });
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

    /// Each record names an entry below its root, whatever it did to it,
    /// a name like a cookie's file among them: a cookie is no file.
    #[test]
    fn records_say_entries() {
        let mut bytes = record(1, r"docs\notes.txt", false);
        bytes.extend(record(2, r"docs\old.txt", false));
        bytes.extend(record(1, r"docs\cookie-8", true));
        let heard = records(&bytes, 3);
        let entry = |rel: &str| Heard::Entry {
            scope: 3,
            rel: PathBuf::from(rel),
        };
        assert_eq!(
            heard,
            vec![
                entry(r"docs\notes.txt"),
                entry(r"docs\old.txt"),
                entry(r"docs\cookie-8")
            ]
        );
        // Cut short: what is whole is said, nothing past the buffer read,
        // and the rest is an overflow: what it held is not known.
        let cut = &bytes[..bytes.len() - 6];
        assert_eq!(
            records(cut, 3),
            vec![
                entry(r"docs\notes.txt"),
                entry(r"docs\old.txt"),
                Heard::Overflow { scope: 3 }
            ]
        );
    }

    /// A name heard by its short form (`LONGFO~1`, as a change made
    /// through a short path may be told) is said by its long one, a folder
    /// and a file alike; one gone since ends the path, its folder read
    /// again; a long name with a tilde is its own. Where the volume makes
    /// no short names, there is nothing to resolve.
    #[test]
    fn a_short_name_heard_is_said_by_its_long_one() {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let s = skwird_testkit::Scratch::new("watch-short");
        s.file(r"root\a long folder name\a long file name.txt");
        s.file(r"root\keep~1.txt");
        let root = s.join("root");
        let long = root.join(r"a long folder name\a long file name.txt");
        let wide: Vec<u16> = long.as_os_str().encode_wide().chain([0]).collect();
        let mut short = vec![0u16; 1024];
        // SAFETY: a NUL-terminated path; the buffer and its length.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), short.as_mut_ptr(), 1024) } as usize;
        assert!(n > 0 && n < 1024, "{}", std::io::Error::last_os_error());
        // Its last two names: the folder's and the file's, short where the
        // volume made them so.
        let short = PathBuf::from(OsString::from_wide(&short[..n]));
        let names: Vec<_> = short.components().rev().take(2).collect();
        let rel: PathBuf = names.into_iter().rev().collect();
        if rel == Path::new(r"a long folder name\a long file name.txt") {
            eprintln!("not proven here: the volume makes no short names");
            return;
        }
        assert_eq!(
            long_names(&root, &rel),
            Path::new(r"a long folder name\a long file name.txt"),
            "{rel:?}"
        );
        let folder = rel.parent().unwrap().to_path_buf();
        assert_eq!(
            long_names(&root, &folder.join("gone.txt")),
            Path::new(r"a long folder name\gone.txt")
        );
        assert_eq!(
            long_names(&root, Path::new(r"GONE~1\below\it.txt")),
            Path::new(r"GONE~1")
        );
        assert_eq!(
            long_names(&root, Path::new("keep~1.txt")),
            Path::new("keep~1.txt")
        );
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

    /// A root whose folder is renamed says its watch ended: its path leads
    /// to nothing heard, though every change below the folder is still
    /// told. So is one whose parent is renamed, at the next change below
    /// it.
    #[test]
    fn a_root_renamed_says_its_watch_ended() {
        let s = Scratch::new("watch-renamed");
        s.file(r"above\root\a\file");
        s.dir(r"top\root");
        let w = Arc::new(Watcher::new(1, 0).unwrap());
        w.watch_root(2, &s.join(r"above\root")).unwrap();
        w.watch_root(3, &s.join(r"top\root")).unwrap();
        std::fs::rename(s.join(r"above\root"), s.join(r"above\moved")).unwrap();
        std::fs::rename(s.join("top"), s.join("top moved")).unwrap();
        // Wine tells a handle's path as it was opened, whatever renamed its
        // folder since: the end is proven on Windows alone.
        if skwird_testkit::under_wine() {
            eprintln!("not proven under Wine: a renamed root's watch ends");
            return;
        }
        s.file(r"top moved\root\made.txt");
        heard_until(&w, |all| {
            all.contains(&Heard::Lost { scope: 2 }) && all.contains(&Heard::Lost { scope: 3 })
        });
    }
}

#[cfg(test)]
mod cookies {
    use std::time::{Duration, Instant};

    use skwird_testkit::Scratch;

    use super::*;

    /// A cookie asked of a root is said after every change made below it
    /// before it was asked, with no file made for it: round after round,
    /// each right after its changes, a change told and read in the same
    /// instant as its cookie is asked. None is asked of a root not heard,
    /// nor under Wine, which tells changes late.
    #[test]
    fn a_cookie_is_said_after_every_change_before_it() {
        let s = Scratch::new("watch-cookie");
        s.dir(r"root\a");
        let w = Arc::new(Watcher::new(1, 0).unwrap());
        w.watch_root(5, &s.join("root")).unwrap();
        assert!(!w.cookie(6, 0), "asked of a root not heard");
        if skwird_testkit::under_wine() {
            assert!(!w.cookie(5, 0), "asked under Wine");
            return;
        }
        let (send, got) = std::sync::mpsc::channel();
        {
            let w = Arc::clone(&w);
            std::thread::spawn(move || {
                loop {
                    let mut heard = Vec::new();
                    if w.read(&mut heard).is_err() || send.send(heard).is_err() {
                        return;
                    }
                }
            });
        }
        let mut all = Vec::new();
        for round in 0..100u64 {
            let made: Vec<PathBuf> = (0..10)
                .map(|i| {
                    let rel = format!(r"a\round {round} file {i}.txt");
                    s.file(format!(r"root\{rel}"));
                    PathBuf::from(rel)
                })
                .collect();
            assert!(w.cookie(5, round));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !all.contains(&Heard::Cookie(round)) {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "round {round}: no cookie");
                if let Ok(heard) = got.recv_timeout(left) {
                    all.extend(heard);
                }
            }
            let at = all.iter().position(|h| *h == Heard::Cookie(round)).unwrap();
            for rel in &made {
                assert!(
                    all[..at]
                        .iter()
                        .any(|h| matches!(h, Heard::Entry { scope: 5, rel: r } if r == rel)),
                    "round {round}: {rel:?} not heard before its cookie"
                );
            }
            assert!(
                !all.iter()
                    .any(|h| matches!(h, Heard::Overflow { .. } | Heard::Lost { .. })),
                "round {round}: {all:?}"
            );
        }
    }
}
