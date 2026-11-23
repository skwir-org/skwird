//! What skwird's tests share (`docs/PLAN.md`, phase 1, step 1):
//!
//! - [`Scratch`]: a synthetic tree in a folder of the test's own, under
//!   the user's cache (`~/.cache/skwird-tests/`), never `/tmp`, which may
//!   be a tmpfs with other rules for change times;
//! - [`reference_walk`]: a plain walk of a tree, one folder after the
//!   other, to compare an answer with;
//! - [`machine`]: the machine the tests that need root run in, and its
//!   filesystems (`tools/vm/root.nix`);
//! - [`in_namespace`]: a test run again in a user and mount namespace of
//!   its own, where it may mount without root.
//!
//! Names are synthetic: spaces, Unicode, bytes that are not UTF-8, depth,
//! links and hard links, never a real user's (AGENTS.md, "Privacy and
//! fixtures").

//!
//! On Windows (`windows`), the same scratch folders and reference walk,
//! for the crawl's tests that run on both (`docs/PLAN.md`, phase 4).

#![cfg(any(unix, windows))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// An entry's kind, as the reference walk tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Folder,
    Link,
    Other,
}

/// What the reference walk saw of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub kind: Kind,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
}

/// What a reference walk found under a root.
#[derive(Debug, Default)]
pub struct Walk {
    /// Every entry below the root, by its path relative to the root.
    pub entries: BTreeMap<PathBuf, Seen>,
    /// Folders whose entries it could not read, and folders where another
    /// mount starts, which it does not enter.
    pub not_read: BTreeSet<PathBuf>,
}
