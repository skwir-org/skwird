//! The watcher of a user's engine on macOS (phase 5, step 3): every root
//! heard whole by one `FSEvents` stream, which names each folder whose
//! entries changed, with the cookies' folder in the same stream, so that a
//! cookie is heard in its turn after every root's changes before it.
//!
//! Its calls to the system are fsearch's (`src/fsevents.rs`, MIT), its
//! lessons with them: `CoreServices` loaded on first use, not linked, since
//! `CoreFoundation`'s initializer costs every process that links it about a
//! millisecond, and every `skwird` a client runs would pay it; the
//! callback on a queue of user-initiated priority, which cut fsearch's
//! drops under load; a latency of 0.5 s, `NoDefer` pushing a change alone
//! through at once; a stream torn down stopped, invalidated while still
//! scheduled, then released.
//!
//! A stream's paths are fixed when it is made: a root watched, or let go
//! of, makes the stream again, from the last event the one before gave
//! (`sinceWhen`), so that nothing between the two is missed. Each stream
//! has its number: a cookie proves the queue read only from the current
//! one, the only one that holds every root (DESIGN 2).

// FSEvents' and CoreFoundation's calls, which no crate the workspace
// takes binds.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};

/// What a watcher heard.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Heard {
    /// The folder at `rel` below root `scope` changed: an entry of it
    /// made, removed, renamed, written to or given other attributes. It is
    /// read again.
    Folder { scope: usize, rel: PathBuf },
    /// What root `scope`'s stream heard was dropped, or a folder of it is
    /// to be read again with all below it (`MustScanSubDirs`), or a volume
    /// was mounted below it: nothing heard there proves anything until a
    /// check.
    Overflow { scope: usize },
    /// Root `scope` is no longer heard: its folder moved or removed, or
    /// its volume unmounted.
    Lost { scope: usize },
    /// The cookie of this number was made ([`Watcher::watch_cookies`]):
    /// every change below every root heard before it has been heard.
    Cookie(u64),
}

/// `kFSEventStreamEventFlag…`: what an event says.
const MUST_SCAN_SUBDIRS: u32 = 0x1;
const USER_DROPPED: u32 = 0x2;
const KERNEL_DROPPED: u32 = 0x4;
const EVENT_IDS_WRAPPED: u32 = 0x8;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
/// `kFSEventStreamCreateFlag…`: an isolated change delivered at once, and
/// a watched root's own move or removal said.
const NO_DEFER: u32 = 0x2;
const WATCH_ROOT: u32 = 0x4;
/// `kFSEventStreamEventIdSinceNow`.
const SINCE_NOW: u64 = u64::MAX;
/// `kCFStringEncodingUTF8`.
const UTF8: u32 = 0x0800_0100;
/// How long the system may hold an event to batch it with more.
const LATENCY: f64 = 0.5;
/// The name a cookie's folder has, its number after it.
const COOKIE: &str = "cookie-";

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

/// `FSEvents`' and `CoreFoundation`'s calls, loaded once.
struct Api {
    create: unsafe extern "C" fn(
        *const c_void,
        Callback,
        *const Context,
        *const c_void,
        u64,
        f64,
        u32,
    ) -> *mut c_void,
    set_queue: unsafe extern "C" fn(*mut c_void, *mut c_void),
    start: unsafe extern "C" fn(*mut c_void) -> u8,
    flush_async: unsafe extern "C" fn(*mut c_void) -> u64,
    stop: unsafe extern "C" fn(*mut c_void),
    invalidate: unsafe extern "C" fn(*mut c_void),
    release: unsafe extern "C" fn(*mut c_void),
    current_id: unsafe extern "C" fn() -> u64,
    cf_release: unsafe extern "C" fn(*const c_void),
    string: unsafe extern "C" fn(*const c_void, *const libc::c_char, u32) -> *const c_void,
    array: unsafe extern "C" fn(
        *const c_void,
        *const *const c_void,
        isize,
        *const c_void,
    ) -> *const c_void,
    array_callbacks: *const c_void,
}

// SAFETY: function pointers, and the address of a constant the system
// never changes: all of it may be shared between threads.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const libc::c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_queue_attr_make_with_qos_class(
        attr: *const c_void,
        qos: u32,
        relative: i32,
    ) -> *const c_void;
    fn dispatch_release(object: *mut c_void);
}

/// `FSEvents`' calls, or why they cannot be had.
fn api() -> std::io::Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load)
        .as_ref()
        .ok_or_else(|| std::io::Error::other("CoreServices cannot be loaded"))
}

fn load() -> Option<Api> {
    let open = |path: &CStr| {
        // SAFETY: a NUL-terminated path; the image stays loaded, never
        // closed.
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY) };
        (!h.is_null()).then_some(h)
    };
    let cs = open(c"/System/Library/Frameworks/CoreServices.framework/CoreServices")?;
    let cf = open(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
    // SAFETY: each symbol is the function or constant of that name, of the
    // type its field says, as the system's headers declare it.
    unsafe {
        Some(Api {
            create: sym(cs, c"FSEventStreamCreate")?,
            set_queue: sym(cs, c"FSEventStreamSetDispatchQueue")?,
            start: sym(cs, c"FSEventStreamStart")?,
            flush_async: sym(cs, c"FSEventStreamFlushAsync")?,
            stop: sym(cs, c"FSEventStreamStop")?,
            invalidate: sym(cs, c"FSEventStreamInvalidate")?,
            release: sym(cs, c"FSEventStreamRelease")?,
            current_id: sym(cs, c"FSEventsGetCurrentEventId")?,
            cf_release: sym(cf, c"CFRelease")?,
            string: sym(cf, c"CFStringCreateWithCString")?,
            array: sym(cf, c"CFArrayCreate")?,
            array_callbacks: sym(cf, c"kCFTypeArrayCallBacks")?,
        })
    }
}

/// A symbol of a loaded image, as `T`: a function pointer, or a constant's
/// address.
///
/// # Safety
///
/// `T` must be a pointer-sized type the symbol is.
unsafe fn sym<T>(image: *mut c_void, name: &CStr) -> Option<T> {
    // SAFETY: a loaded image, a NUL-terminated name.
    let p = unsafe { libc::dlsym(image, name.as_ptr()) };
    // SAFETY: the caller's: `T` is pointer-sized, and the symbol is one.
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy(&p) })
}

/// What was heard, waiting to be read.
#[derive(Default)]
struct Queue {
    heard: Mutex<Vec<Heard>>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, more: Vec<Heard>) {
        if more.is_empty() {
            return;
        }
        self.heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(more);
        self.ready.notify_all();
    }
}

/// What every stream shares with its watcher.
#[derive(Default)]
struct Shared {
    queue: Queue,
    /// The current stream's number.
    current: AtomicU64,
    /// The last event the current stream gave: where the next starts.
    last: AtomicU64,
}

/// What a stream's callback is given: its paths as they were when it was
/// made.
struct Info {
    shared: Arc<Shared>,
    number: u64,
    roots: Vec<(usize, PathBuf)>,
    cookies: Option<PathBuf>,
}

/// A stream running; dropped, it is stopped, invalidated and released, and
/// its callback is called no more.
struct Stream {
    stream: *mut c_void,
    queue: *mut c_void,
}

// SAFETY: a stream is started and stopped by its owner alone, under the
// watcher's lock; its callback runs on its own queue.
unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        let Ok(a) = api() else { return };
        // SAFETY: a stream made and started here; stopped, invalidated
        // while still scheduled, released, in that order (fsearch's); its
        // queue let go of after, the stream holding it no more.
        unsafe {
            (a.stop)(self.stream);
            (a.invalidate)(self.stream);
            (a.release)(self.stream);
            dispatch_release(self.queue);
        }
    }
}

/// What the watcher watches, and its stream.
#[derive(Default)]
struct Watched {
    roots: Vec<(usize, PathBuf)>,
    cookies: Option<PathBuf>,
    stream: Option<Stream>,
}

/// The watcher of a user's engine on macOS: every root heard whole, by
/// one stream.
pub struct Watcher {
    generation: u64,
    shared: Arc<Shared>,
    watched: Mutex<Watched>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Watcher {
    /// A watcher of generation `generation`, where `FSEvents` can be had.
    /// macOS needs no watch for each folder: there is no budget to keep to.
    pub fn new(generation: u64, _budget: usize) -> std::io::Result<Watcher> {
        api()?;
        Ok(Watcher {
            generation,
            shared: Arc::default(),
            watched: Mutex::new(Watched::default()),
        })
    }

    /// No share of watches to keep to on macOS.
    pub fn share() -> usize {
        0
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Hear every change below the root `scope` at `path` from now on.
    /// Its stream made again, it is heard once this returns.
    pub fn watch_root(&self, scope: usize, path: &Path) -> std::io::Result<()> {
        let mut w = self.watched.lock().unwrap_or_else(PoisonError::into_inner);
        w.roots.retain(|(s, _)| *s != scope);
        w.roots.push((scope, path.to_path_buf()));
        self.restart(&mut w)
    }

    /// Hear nothing of root `scope` any more.
    pub fn unwatch_root(&self, scope: usize) {
        let mut w = self.watched.lock().unwrap_or_else(PoisonError::into_inner);
        let before = w.roots.len();
        w.roots.retain(|(s, _)| *s != scope);
        if w.roots.len() != before {
            let _ = self.restart(&mut w);
        }
    }

    /// Say a folder `cookie-N` changed in `dir` (a file made in it) as
    /// [`Heard::Cookie`], in its turn: `dir` is heard in the roots' stream.
    pub fn watch_cookies(&self, dir: &Path) -> std::io::Result<()> {
        // As the system names it in its events: `/var` is `/private/var`.
        let dir = std::fs::canonicalize(dir)?;
        let mut w = self.watched.lock().unwrap_or_else(PoisonError::into_inner);
        w.cookies = Some(dir);
        self.restart(&mut w)
    }

    /// What the stream holds back of its latency, given now: a cookie just
    /// made is heard at once.
    pub fn flush(&self) {
        let w = self.watched.lock().unwrap_or_else(PoisonError::into_inner);
        if let (Some(s), Ok(a)) = (&w.stream, api()) {
            // SAFETY: a stream made and started here, alive under the lock.
            unsafe { (a.flush_async)(s.stream) };
        }
    }

    /// The stream made again for what is watched now: from the last event
    /// the one before gave, so that nothing between the two is missed
    /// (what both give is read twice, which is never wrong); then the one
    /// before stopped. None where nothing is watched.
    fn restart(&self, w: &mut Watched) -> std::io::Result<()> {
        let a = api()?;
        let old = w.stream.take();
        if w.roots.is_empty() {
            return Ok(());
        }
        let since = if old.is_some() {
            self.shared.last.load(Ordering::Acquire)
        } else {
            // SAFETY: a call that takes nothing.
            let now = unsafe { (a.current_id)() };
            self.shared.last.store(now, Ordering::Release);
            SINCE_NOW
        };
        let number = self.shared.current.fetch_add(1, Ordering::AcqRel) + 1;
        let info = Box::new(Info {
            shared: Arc::clone(&self.shared),
            number,
            roots: w.roots.clone(),
            cookies: w.cookies.clone(),
        });
        let mut paths: Vec<CString> = Vec::new();
        for p in w.roots.iter().map(|(_, p)| p).chain(w.cookies.as_ref()) {
            paths.push(CString::new(p.as_os_str().as_bytes())?);
        }
        let stream = make(a, info, &paths, since)?;
        w.stream = Some(stream);
        drop(old);
        Ok(())
    }

    /// Wait until something was heard, then take it all.
    pub fn read(&self, out: &mut Vec<Heard>) -> std::io::Result<()> {
        let mut heard = self
            .shared
            .queue
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while heard.is_empty() {
            heard = self
                .shared
                .queue
                .ready
                .wait(heard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        out.append(&mut heard);
        Ok(())
    }

    // What Linux's watcher has for its watches of each folder, which macOS
    // does not take: nothing to keep, nothing to let go of.

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

/// A stream of `paths` from `since`, started, its callback given `info`.
fn make(a: &Api, info: Box<Info>, paths: &[CString], since: u64) -> std::io::Result<Stream> {
    let strings: Vec<*const c_void> = paths
        .iter()
        // SAFETY: the default allocator, a NUL-terminated name in UTF-8,
        // as macOS names files.
        .map(|p| unsafe { (a.string)(std::ptr::null(), p.as_ptr(), UTF8) })
        .collect();
    if strings.iter().any(|s| s.is_null()) {
        for s in strings.into_iter().filter(|s| !s.is_null()) {
            // SAFETY: a string made above, ours to release.
            unsafe { (a.cf_release)(s) };
        }
        return Err(std::io::Error::other("a path cannot be given to FSEvents"));
    }
    let count = isize::try_from(strings.len()).unwrap_or(isize::MAX);
    // SAFETY: the default allocator; `count` strings, which the array
    // retains (CFType's callbacks).
    let array = unsafe { (a.array)(std::ptr::null(), strings.as_ptr(), count, a.array_callbacks) };
    for &s in &strings {
        // SAFETY: made above; the array holds its own reference.
        unsafe { (a.cf_release)(s) };
    }
    if array.is_null() {
        return Err(std::io::Error::other("FSEvents' paths cannot be made"));
    }
    let context = Context {
        version: 0,
        info: Box::into_raw(info).cast(),
        retain: std::ptr::null(),
        release: Some(release_info),
        copy_description: std::ptr::null(),
    };
    // SAFETY: the default allocator, our callback, a context the stream
    // copies (its info released by `release_info` with the stream), the
    // paths, which the stream retains.
    let stream = unsafe {
        (a.create)(
            std::ptr::null(),
            on_events,
            &raw const context,
            array,
            since,
            LATENCY,
            NO_DEFER | WATCH_ROOT,
        )
    };
    // SAFETY: made above; the stream holds its own reference.
    unsafe { (a.cf_release)(array) };
    if stream.is_null() {
        release_info(context.info);
        return Err(std::io::Error::other("an FSEvents stream cannot be made"));
    }
    // The callback copies paths out alone, but must not wait behind busy
    // threads: the system drops what a client falls behind on (fsearch).
    // SAFETY: a label and an attribute the calls make; the queue is ours,
    // released with the stream.
    let queue = unsafe {
        let attr = dispatch_queue_attr_make_with_qos_class(
            std::ptr::null(),
            libc::qos_class_t::QOS_CLASS_USER_INITIATED as u32,
            0,
        );
        dispatch_queue_create(c"skwird.fsevents".as_ptr(), attr)
    };
    let started = Stream { stream, queue };
    // SAFETY: the stream made above, scheduled on its queue, then started.
    let ok = unsafe {
        (a.set_queue)(stream, queue);
        (a.start)(stream)
    };
    if ok == 0 {
        return Err(std::io::Error::other(
            "an FSEvents stream cannot be started",
        ));
    }
    Ok(started)
}

extern "C" fn release_info(info: *const c_void) {
    if !info.is_null() {
        // SAFETY: the info `make` gave the stream, from a box, released
        // once, as the stream is.
        drop(unsafe { Box::from_raw(info.cast_mut().cast::<Info>()) });
    }
}

/// What a stream gives: each event said as what was heard, and queued.
extern "C" fn on_events(
    _: *mut c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    // SAFETY: the info `make` gave the stream, alive while it calls.
    let info = unsafe { &*info.cast::<Info>() };
    let paths = paths.cast::<*const libc::c_char>();
    let mut heard = Vec::new();
    let mut last = 0;
    for i in 0..count {
        // SAFETY: the stream gives `count` paths, flags and ids, each path
        // NUL-terminated (no CFTypes asked).
        let (path, flag, id) = unsafe {
            (
                CStr::from_ptr(*paths.add(i)).to_bytes(),
                *flags.add(i),
                *ids.add(i),
            )
        };
        last = last.max(id);
        said(info, path, flag, &mut heard);
    }
    let current = info.shared.current.load(Ordering::Acquire) == info.number;
    if current {
        info.shared.last.fetch_max(last, Ordering::AcqRel);
    } else {
        // An older stream's cookie proves nothing of the roots it did not
        // hold (DESIGN 2); its changes are read all the same.
        heard.retain(|h| !matches!(h, Heard::Cookie(_)));
    }
    info.shared.queue.push(heard);
}

/// What one event says, put in `heard`.
fn said(info: &Info, path: &[u8], flag: u32, heard: &mut Vec<Heard>) {
    if flag & HISTORY_DONE != 0 {
        return;
    }
    let every = |heard: &mut Vec<Heard>, h: fn(usize) -> Heard| {
        heard.extend(info.roots.iter().map(|&(scope, _)| h(scope)));
    };
    // Dropped, by the kernel or the service: every root's.
    if flag & (USER_DROPPED | KERNEL_DROPPED | EVENT_IDS_WRAPPED) != 0 {
        every(heard, |scope| Heard::Overflow { scope });
        return;
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(
        path.strip_suffix(b"/")
            .filter(|p| !p.is_empty())
            .unwrap_or(path),
    ));
    if let Some(cookies) = &info.cookies
        && let Ok(rel) = path.strip_prefix(cookies)
    {
        if let Some(n) = rel
            .to_str()
            .and_then(|r| r.strip_prefix(COOKIE))
            .and_then(|n| n.parse().ok())
        {
            heard.push(Heard::Cookie(n));
        }
        return;
    }
    for (scope, root) in &info.roots {
        let scope = *scope;
        // A root's own folder moved or removed, or a volume gone at or
        // above it: no longer heard.
        if (flag & ROOT_CHANGED != 0 || flag & UNMOUNT != 0) && root.starts_with(path) {
            heard.push(Heard::Lost { scope });
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        if flag & (MUST_SCAN_SUBDIRS | MOUNT | UNMOUNT) != 0 {
            heard.push(Heard::Overflow { scope });
        } else {
            heard.push(Heard::Folder {
                scope,
                rel: rel.to_path_buf(),
            });
        }
    }
}

/// The folder a cookie numbered `n` is made in: a file made in it is an
/// event that names it.
pub fn cookie_folder(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{COOKIE}{n}"))
}
