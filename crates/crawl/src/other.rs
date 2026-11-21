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

#[derive(Clone, Debug)]
pub struct Stored {
    pub rel: PathBuf,
    pub id: FileId,
    pub ctime: Time,
    pub again: bool,
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

pub fn check(root: &Root, _: &[Stored], _: &Options, _: &Cancel) -> Result<Checked, Error> {
    match root.never {}
}
