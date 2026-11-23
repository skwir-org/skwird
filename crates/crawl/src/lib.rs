//! skwird's crawl (`docs/DESIGN.md`, decision 2): every folder under a
//! root read, and the check that proves the names read are still current
//! without any watch.
//!
//! - [`crawl`] reads every folder below a root, or below one of its
//!   folders, on threads of its own, and hands each folder to a sink as it
//!   is read.
//! - [`read_folder`] reads one folder again.
//! - [`Watcher`] watches folders as they are read (inotify, phase 2): a
//!   folder is watched through its open handle before its entries are
//!   read, so the folder watched is the folder read, and a change
//!   between the two is heard.
//! - [`check`] compares each folder's identity and change time with what
//!   was stored, and says which to read again.
//!
//! What holds throughout:
//!
//! - **A folder's change time is read before its entries,** so a change
//!   during the read leaves a time that differs.
//! - **A folder read within a tick of its last change is read again** at
//!   the next check ([`Listing::again`]), since a second change within the
//!   tick can leave its change time as it was.
//! - **No link is followed,** and no mount is crossed: a folder is opened
//!   by a path beneath the root that may hold neither
//!   (`openat2` with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS` and
//!   `RESOLVE_NO_XDEV`), so another folder's names are never read under
//!   its path. Depth does not matter: a path longer than the system allows
//!   is opened a part at a time.
//! - **Mounts come from the mount table** ([`mounts`]), never from reading
//!   a path: a mount point below the root is not stat-ed, and is not
//!   covered. The mount id is the backstop for a mount made since.
//! - **A failed read is not an empty folder:** a folder that cannot be
//!   read, or whose listing fails part-way, is not covered ([`Why`]).
//! - **Private folders are never read into what a crawl gives** (decision
//!   5): an exclusion, or a `.private` file in the folder.
//!
//! On systems other than Linux, [`Root::open`] answers that the crawl
//! does not read their folders yet (rule 7).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub mod acl;
pub mod exclude;
pub mod fanotify;
pub mod mounts;
pub mod wtf8;

// What every system does alike (`walk`), and what each does its own way
// (`sys`): opening, listing and stat-ing a folder.
mod walk;
pub use walk::{
    Checked, Crawl, Reader, Reuse, Stale, Stored, StoredFolder, check, check_watched, crawl,
    crawl_all, crawl_below, crawl_below_by, crawl_by, read_folder, read_folder_reusing,
    read_folder_watched,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "linux")]
pub use linux::watch::{Heard, Watcher};
#[cfg(target_os = "linux")]
pub use linux::{Root, filesystem_name};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;
#[cfg(windows)]
pub use windows::Root;
#[cfg(windows)]
pub use windows::watch::{Heard, Watcher};
#[cfg(windows)]
pub use windows::{OPEN_TO_ALL, descriptor_on, journal, mft};

#[cfg(not(any(target_os = "linux", windows)))]
mod other;
#[cfg(not(any(target_os = "linux", windows)))]
use other as sys;
#[cfg(not(any(target_os = "linux", windows)))]
pub use other::Root;

// No watcher but Linux's and Windows' yet (rule 7): macOS's comes with
// phase 5.
#[cfg(not(any(target_os = "linux", windows)))]
mod unwatched;
#[cfg(not(any(target_os = "linux", windows)))]
pub use unwatched::{Heard, Watcher};

pub use exclude::Exclusions;

/// `path` resolved by the system, named as skwird names paths: on Windows
/// plainly (`C:\Users`), not in the verbatim form the system gives
/// (`\\?\C:\Users`), as clients name them.
pub fn canonical(path: impl AsRef<std::path::Path>) -> std::io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    let resolved = windows::plain(&resolved);
    Ok(resolved)
}

/// A file's identity: its device and inode. On btrfs each subvolume has
/// a device of its own, so the two name one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// An instant, as the system's clock and its files' times give it:
/// seconds since 1970 and nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

impl Time {
    /// Now, by the clock files' times are taken from.
    pub fn now() -> Time {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Time {
            sec: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            nsec: since.subsec_nanos(),
        }
    }

    /// This instant and `d` after it, at the latest instant if it would
    /// overflow.
    #[must_use]
    pub fn plus(self, d: Duration) -> Time {
        let nsec = u64::from(self.nsec) + u64::from(d.subsec_nanos());
        let carry = i64::try_from(nsec / 1_000_000_000).unwrap_or(0);
        let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        Time {
            sec: self.sec.saturating_add(secs).saturating_add(carry),
            nsec: (nsec % 1_000_000_000) as u32,
        }
    }
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    File,
    Folder,
    Link,
    /// Pipes, sockets, devices.
    Other,
}

/// What a stat says of an entry beyond its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    pub size: u64,
    /// The bytes the filesystem holds for it.
    pub alloc: u64,
    pub mtime: Time,
    pub ctime: Time,
}

/// An entry of a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub id: FileId,
    /// Its attributes, where a stat gave them: none for a mount point
    /// (never stat-ed), nor where the crawl was asked for names only.
    pub attrs: Option<Attrs>,
}

/// A folder's own facts, as its stat gave them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderStat {
    pub id: FileId,
    /// The mount it is in, by its mount id.
    pub mount: u64,
    pub ctime: Time,
    pub mtime: Time,
    pub owner: u32,
    pub group: u32,
    /// Its permission bits.
    pub mode: u16,
}

/// A folder read: its facts, taken before its entries, and its entries.
#[derive(Clone, Debug)]
pub struct Listing {
    pub stat: FolderStat,
    /// When its stat was taken.
    pub read_at: Time,
    /// Whether it must be read again at the next check whatever its change
    /// time says: it was read within a tick of its last change.
    pub again: bool,
    names: Vec<u8>,
    ends: Vec<u32>,
    entries: Vec<Entry>,
}

impl Listing {
    // Only Linux's crawl builds listings so far (rule 7: the others'
    // answer that they read no folder yet).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn new(stat: FolderStat, read_at: Time, again: bool) -> Listing {
        Listing {
            stat,
            read_at,
            again,
            names: Vec::new(),
            ends: Vec::new(),
            entries: Vec::new(),
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn push(&mut self, name: &[u8], entry: Entry) {
        self.names.extend_from_slice(name);
        self.ends.push(self.names.len() as u32);
        self.entries.push(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `i`th entry's name, as the filesystem holds it.
    pub fn name(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.names[start..self.ends[i] as usize]
    }

    pub fn entry(&self, i: usize) -> &Entry {
        &self.entries[i]
    }

    /// Each entry, with its name.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Entry)> {
        (0..self.len()).map(|i| (self.name(i), self.entry(i)))
    }

    /// Whether its names are in the order of their bytes, as a crawl's read
    /// pushes them.
    pub fn is_sorted(&self) -> bool {
        (1..self.len()).all(|i| self.name(i - 1) < self.name(i))
    }
}

/// Why a folder's entries are not covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Why {
    /// An exclusion names it (decision 5).
    Excluded,
    /// It holds a `.private` file (decision 5).
    Private,
    /// Its entries may not be read.
    Denied,
    /// Its read failed, with this error number.
    Failed(i32),
    /// Its listing failed part-way, with this error number: what was read
    /// of it is not kept.
    CutShort(i32),
    /// It was there when its parent was read, and is not now.
    Gone,
    /// Its path now goes through a link, or leads to another folder.
    Replaced,
    /// Another mount starts there.
    Mount,
    /// A network mount starts there: never crawled (decision 2).
    NetworkMount,
    /// A btrfs snapshot below the root, a read-only subvolume: left out,
    /// each another copy of what it was taken of (open question 10).
    Snapshot,
    /// Found, not read yet: a folder made since its parent was read,
    /// waiting for its crawl.
    Unread,
}

impl Why {
    /// Whether the next check reads it again whatever its times say: what
    /// failed may succeed, as a permission given through a group changes
    /// no folder's time.
    pub fn retried(self) -> bool {
        matches!(
            self,
            Why::Denied
                | Why::Failed(_)
                | Why::CutShort(_)
                | Why::Gone
                | Why::Replaced
                | Why::Unread
        )
    }
}

/// What came of a folder.
#[derive(Clone, Debug)]
pub enum Outcome {
    Listed(Listing),
    NotCovered(Why),
}

/// A folder a crawl reached.
#[derive(Clone, Debug)]
pub struct Crawled {
    /// Its path below the root (empty for the root).
    pub rel: PathBuf,
    /// The slot of the folder holding it, in the same crawl: none for the
    /// folder the crawl started at.
    pub parent: Option<usize>,
    /// Its facts where known: from its own stat when it was read, or from
    /// its parent's stat of it when it was not.
    pub stat: Option<FolderStat>,
    pub outcome: Outcome,
    /// What came of watching it ([`Options::watcher`]).
    pub watch: Watch,
}

/// What came of watching a folder as it was read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Watch {
    /// No watch was asked for it: no watcher, or the folder is outside
    /// every subtree the watcher watches, or it was not read.
    #[default]
    Unasked,
    /// Watched, by this descriptor of the watcher's instance.
    Watched(i32),
    /// Not watched: the watcher's budget of watches is spent.
    Budget,
    /// Not watched: the user's limit of watches is reached.
    Limit,
    /// Not watched, with this error number.
    Failed(i32),
}

/// How a crawl or a check runs.
#[derive(Clone, Debug)]
pub struct Options {
    /// A watcher, for every folder a crawl or a reread reads within its
    /// subtrees to be watched before its entries are read.
    pub watcher: Option<Arc<Watcher>>,
    pub threads: usize,
    /// Stat every entry, for its attributes. Without, only folders are
    /// stat-ed (their identity and mount), and entries carry no
    /// attributes.
    pub attrs: bool,
    /// How close to its last change a folder may be read and still be
    /// trusted: within it, it is read again at the next check. Two
    /// seconds covers the coarsest change times local filesystems keep.
    pub tick: Duration,
    /// Say of each folder read whether it has an access ACL ([`ACL`] in
    /// its mode), as the system service needs to answer each asker
    /// (DESIGN 4): one more call a folder. On Windows, each folder's
    /// security id instead, as its owner, and the root fetches the
    /// descriptor of each id met ([`Root::descriptor`]).
    pub acls: bool,
}

/// The bit of a folder's mode, above its permission bits, that says it
/// has an access ACL ([`Options::acls`]); one whose ACL could not be read
/// says so too, so that it is never answered as its mode alone says.
pub const ACL: u16 = 0o10000;

impl Default for Options {
    fn default() -> Self {
        Options {
            watcher: None,
            threads: std::thread::available_parallelism().map_or(4, std::num::NonZero::get),
            attrs: true,
            tick: Duration::from_secs(2),
            acls: false,
        }
    }
}

/// A crawl or a check asked to stop, from any thread.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Why a crawl or a check could not run.
#[derive(Debug)]
pub enum Error {
    /// The crawl does not read this system's folders yet.
    Unsupported(&'static str),
    /// The root cannot be opened or stat-ed.
    Root(std::io::Error),
    /// The root is on a network mount, which skwird never crawls.
    Network,
    /// The kernel lacks a call the crawl needs.
    Kernel(&'static str),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(system) => write!(
                f,
                "skwird does not read {system}'s folders yet: it reads Linux's \
                 (macOS comes with phase 5, Windows with phase 4)"
            ),
            Error::Root(e) => write!(f, "the root cannot be read: {e}"),
            Error::Network => write!(
                f,
                "the root is on a network mount, which skwird never crawls"
            ),
            Error::Kernel(what) => write!(f, "this kernel lacks {what}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}
