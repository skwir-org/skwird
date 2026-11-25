//! The crawl's answer on systems that read no folder (rule 7): the same
//! calls, and a root that cannot be opened, with the system named.

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use crate::mounts::Table;
use crate::walk::{Reuse, Stale, StoredFolder};
use crate::{Error, Exclusions, FileId, FolderStat, Options, Outcome, Watch};

const SYSTEM: &str = "this system";

/// A root, which this system does not open yet.
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

    pub fn times_tell(&self) -> bool {
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

    pub(crate) fn mount_at(&self, _: &Path) -> Option<bool> {
        match self.never {}
    }

    pub(crate) fn points(&self) -> Vec<(PathBuf, bool)> {
        match self.never {}
    }
}

pub(crate) fn read(
    root: &Root,
    _: &Path,
    _: &Options,
    _: &mut Watch,
    _: Option<Reuse<'_>>,
) -> (Option<FolderStat>, Outcome) {
    match root.never {}
}

pub(crate) fn verdict(
    root: &Root,
    _: &[(PathBuf, bool)],
    _: &Path,
    _: StoredFolder,
) -> Option<Stale> {
    match root.never {}
}

pub(crate) fn verdict_watched(
    root: &Root,
    _: &[(PathBuf, bool)],
    _: &Path,
    _: StoredFolder,
    _: &Options,
) -> (Option<Stale>, Watch) {
    match root.never {}
}

/// A name's bytes as a path component.
#[cfg(unix)]
pub(crate) use crate::mounts::component;

/// A name's bytes, its WTF-8 ([`crate::wtf8`]), as a path component.
#[cfg(windows)]
pub(crate) fn component(name: &[u8]) -> PathBuf {
    crate::wtf8::path(name)
}

/// `path`'s bytes, put at the end of `buf`.
#[cfg(unix)]
pub(crate) fn push_path(buf: &mut Vec<u8>, path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    buf.extend_from_slice(path.as_os_str().as_bytes());
}

/// `path`'s bytes, its WTF-8, put at the end of `buf`.
#[cfg(windows)]
pub(crate) fn push_path(buf: &mut Vec<u8>, path: &Path) {
    crate::wtf8::push(path, buf);
}

/// The path `bytes` name, as [`push_path`] put them.
#[cfg(unix)]
pub(crate) fn path_of(bytes: &[u8]) -> std::borrow::Cow<'_, Path> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

/// The path `bytes` name, as [`push_path`] put them.
#[cfg(windows)]
pub(crate) fn path_of(bytes: &[u8]) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Owned(crate::wtf8::path(bytes))
}
