//! The watcher's answer on systems that have none yet (rule 7): the same
//! calls, and a watcher that cannot be made, with the system named.
//! Windows' comes with phase 4 (`ReadDirectoryChangesW`), macOS's with
//! phase 5 (`FSEvents`).

use std::convert::Infallible;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const SYSTEM: &str = "macOS";
#[cfg(windows)]
const SYSTEM: &str = "Windows";
#[cfg(not(any(target_os = "macos", windows)))]
const SYSTEM: &str = "this system";

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

    pub fn budget(&self) -> usize {
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
