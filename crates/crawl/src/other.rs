//! The crawl's answer on systems other than Linux (rule 7): the same
//! calls, and a root that cannot be opened, with the system named. macOS
//! comes with phase 5 (`getattrlistbulk`), Windows with phase 4.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::mounts::Table;
use crate::{Cancel, Crawled, Error, Exclusions, FileId, FolderStat, Options, Outcome, Time};

#[cfg(target_os = "macos")]
const SYSTEM: &str = "macOS";
#[cfg(windows)]
const SYSTEM: &str = "Windows";
#[cfg(not(any(target_os = "macos", windows)))]
const SYSTEM: &str = "this system";

/// A root, which no system but Linux opens yet.
pub struct Root {
    never: Infallible,
}

impl Root {
    pub fn open(_: &Path, _: Exclusions) -> Result<Root, Error> {
        Err(Error::Unsupported(SYSTEM))
    }

    pub fn open_with(_: &Path, _: Exclusions, _: Table) -> Result<Root, Error> {
        Err(Error::Unsupported(SYSTEM))
    }

    pub fn path(&self) -> &Path {
        match self.never {}
    }

    pub fn let_go(&self) {
        match self.never {}
    }

    pub fn access_acl(&self, _: &Path, _: FileId) -> Option<crate::acl::Acl> {
        match self.never {}
    }

    pub fn stat(&self) -> &FolderStat {
        match self.never {}
    }

    pub fn fs_name(&self) -> &str {
        match self.never {}
    }

    pub fn table(&self) -> &Table {
        match self.never {}
    }

    pub fn exclusions(&self) -> &Exclusions {
        match self.never {}
    }

    pub fn set_exclusions(&mut self, _: Exclusions) {
        match self.never {}
    }

    pub fn mounts_below(&self) -> Vec<PathBuf> {
        match self.never {}
    }

    pub fn reread_mounts(&mut self) -> std::io::Result<()> {
        match self.never {}
    }

    pub fn set_table(&mut self, _: Table) {
        match self.never {}
    }
}

pub fn read_folder(root: &Root, _: &Path, _: &Options) -> (Option<FolderStat>, Outcome) {
    match root.never {}
}

pub type Reuse<'a> = &'a dyn Fn(&[u8], u64, crate::Kind) -> Option<crate::Entry>;

pub fn read_folder_reusing(
    root: &Root,
    _: &Path,
    _: &Options,
    _: Reuse<'_>,
) -> (Option<FolderStat>, Outcome, crate::Watch) {
    match root.never {}
}

pub fn read_folder_watched(
    root: &Root,
    _: &Path,
    _: &Options,
) -> (Option<FolderStat>, Outcome, crate::Watch) {
    match root.never {}
}

pub fn crawl(
    root: &Root,
    _: &Path,
    _: &Options,
    _: &Cancel,
    _: &(dyn Fn(usize, Crawled) + Sync),
) -> Result<Duration, Error> {
    match root.never {}
}

pub struct Crawl {
    pub folders: Vec<Crawled>,
    pub took: Duration,
}

pub fn crawl_all(root: &Root, _: &Options, _: &Cancel) -> Result<Crawl, Error> {
    match root.never {}
}

pub fn crawl_below(root: &Root, _: &Path, _: &Options, _: &Cancel) -> Result<Crawl, Error> {
    match root.never {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoredFolder {
    pub id: FileId,
    pub ctime: Time,
    pub again: bool,
}

/// The folders an index stored, as Linux's crawl keeps them, a path each
/// here: no check runs on this system yet.
#[derive(Clone, Debug, Default)]
pub struct Stored {
    folders: Vec<(PathBuf, StoredFolder)>,
}

impl Stored {
    pub fn with_capacity(folders: usize, _bytes: usize) -> Stored {
        Stored {
            folders: Vec::with_capacity(folders),
        }
    }

    pub fn push(&mut self, rel: &Path, folder: StoredFolder) {
        self.folders.push((rel.to_path_buf(), folder));
    }

    pub fn len(&self) -> usize {
        self.folders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    pub fn rel(&self, i: usize) -> &Path {
        &self.folders[i].0
    }

    pub fn folder(&self, i: usize) -> StoredFolder {
        self.folders[i].1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    Changed,
    Replaced,
    Gone,
    Again,
    Denied,
    Mount,
    NetworkMount,
    Failed(i32),
}

pub struct Checked {
    pub stale: Vec<(usize, Stale)>,
    pub took: Duration,
}

pub fn check(root: &Root, _: &Stored, _: &Options, _: &Cancel) -> Result<Checked, Error> {
    match root.never {}
}

/// What a watcher heard (Linux's inotify; none here yet).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Heard {
    Changed(i32),
    Entry(i32, Box<[u8]>),
    Moved(i32),
    Gone(i32),
    Overflow,
    Cookie(u64),
}

/// A watcher, which no system but Linux makes yet (rule 7): [`Watcher::new`]
/// says so.
#[derive(Debug)]
pub struct Watcher {
    never: Infallible,
}

impl Watcher {
    pub fn new(_: u64, _: usize) -> std::io::Result<Watcher> {
        Err(std::io::Error::other(format!(
            "skwird watches no folder on {SYSTEM} yet"
        )))
    }

    pub fn share() -> usize {
        0
    }

    pub fn generation(&self) -> u64 {
        match self.never {}
    }

    pub fn watching(&self) -> usize {
        match self.never {}
    }

    pub fn set_scopes(&self, _: Vec<PathBuf>) {
        match self.never {}
    }

    pub fn wants(&self, _: &Path) -> bool {
        match self.never {}
    }

    pub fn watch_cookies(&self, _: &Path) -> std::io::Result<()> {
        match self.never {}
    }

    pub fn unwatch(&self, _: i32) {
        match self.never {}
    }

    pub fn read(&self, _: &mut Vec<Heard>) -> std::io::Result<()> {
        match self.never {}
    }
}

pub fn check_watched(
    root: &Root,
    _: &Stored,
    _: &Options,
    _: &Cancel,
) -> Result<(Checked, Vec<crate::Watch>), Error> {
    match root.never {}
}
