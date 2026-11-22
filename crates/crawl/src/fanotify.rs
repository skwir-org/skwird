//! fanotify (`docs/PLAN.md`, phase 3, step 3): whole filesystems marked
//! for a system service, as phase 0's probe found it works (DESIGN 3).
//!
//! - **A mark is a whole filesystem** (`FAN_MARK_FILESYSTEM`): no watch
//!   per folder, every user's files. It needs `CAP_SYS_ADMIN`.
//! - **btrfs is marked through its top level.** A mark through a
//!   subvolume whose filesystem id is not its superblock's is refused
//!   (`EXDEV`): the filesystem is then mounted again at its top-level
//!   subvolume, read-only and attached to no folder, the mark set through
//!   it, and that mount let go of. The mark is the superblock's, and
//!   hears every subvolume.
//! - **One mark a filesystem,** known by the device the mount table
//!   gives it, the same for every subvolume of one btrfs.
//! - **Events name folders by handle** ([`Folder`]): the filesystem's id
//!   and the folder's file handle. [`Resolver`] opens a handle and says
//!   which folder it is, by its device and inode, as the index keeps
//!   them. A handle that no longer opens (`ESTALE`) is a folder gone: it
//!   was emptied, then removed, and the removal of the topmost folder
//!   removed is heard in its parent, which is still there. Any other
//!   failure proves nothing heard on that filesystem.
//! - **Nothing of a filesystem is held open.** A descriptor kept on one
//!   would keep its user's `umount` from succeeding: handles are opened
//!   through a mount of the filesystem as the mount table names it, found
//!   again when they are read, and known to be that mount by its id.
//! - **One queue for every mark,** in the order things happened: a
//!   cookie made on any marked filesystem comes after every event queued
//!   before it (the barrier, DESIGN 2; open question 1).
//! - **An overflow is said** ([`Heard::Overflow`]), and each group has
//!   its generation, as the inotify watcher's instances do.
//!
//! On other systems, [`Group::new`] says fanotify is Linux's (rule 7).

#[cfg(target_os = "linux")]
#[path = "linux/fanotify.rs"]
mod sys;

pub use sys::{Group, Resolver};

/// A folder as an event names it: the id of the filesystem it is on, as
/// events on that filesystem's mark come with it, and its file handle (a
/// `struct file_handle`: its length, its type, then its bytes).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Folder {
    pub fsid: u64,
    pub handle: Box<[u8]>,
}

/// What a group heard.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Heard {
    /// The entry of this name in this folder was made, removed, moved,
    /// written to or given other attributes: read the folder again,
    /// stat-ing it alone of what was there.
    Entry(Folder, Box<[u8]>),
    /// The folder itself changed, with no entry named: read it again.
    Changed(Folder),
    /// Events were dropped: what was heard proves nothing until a check.
    Overflow,
    /// The cookie of this number was made ([`Group::watch_cookies`]):
    /// every event queued before it has been read.
    Cookie(u64),
}

/// How a filesystem was marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Through {
    /// Through the folder asked.
    Itself,
    /// Through its top-level subvolume, mounted for the mark alone.
    TopLevel,
}

/// A filesystem marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marked {
    /// Its device, as the mount table gives it.
    pub dev: (u32, u32),
    /// The id its events come with.
    pub fsid: u64,
    pub through: Through,
    /// Whether this call marked it, or it was marked already.
    pub new: bool,
}

/// Why a filesystem was not marked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Only a holder of `CAP_SYS_ADMIN` marks a whole filesystem.
    NotPermitted,
    /// The filesystem cannot give file handles (`EOPNOTSUPP`, `ENODEV`).
    NoHandles,
    /// Another filesystem marked has the same id: their events could not
    /// be told apart.
    SameId,
    /// The mount table names no mount holding the folder.
    NoMount,
    /// In a btrfs subvolume, and its top level could not be mounted: why.
    TopLevel(String),
    /// Another error, by its number.
    Failed(i32),
}

/// What a folder's handle opened to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The folder of this identity.
    Is(crate::FileId),
    /// The folder is gone (`ESTALE`): its removal is heard in a parent.
    Gone,
    /// No mark of this group has the handle's filesystem id.
    Unmarked,
    /// It could not be opened, by this error number: nothing heard on its
    /// filesystem proves anything.
    Failed(i32),
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use std::path::Path;

    use super::{Folder, Heard, Marked, Refused, Resolved};
    use crate::mounts::Table;

    fn unsupported() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "fanotify is Linux's: this system's service is a phase of its own",
        )
    }

    /// fanotify is Linux's.
    #[derive(Debug)]
    pub struct Group(());

    impl Group {
        pub fn new(_generation: u64) -> std::io::Result<Group> {
            Err(unsupported())
        }

        pub fn generation(&self) -> u64 {
            0
        }

        pub fn mark(&self, _folder: &Path, _table: &Table) -> Result<Marked, Refused> {
            Err(Refused::NotPermitted)
        }

        pub fn watch_cookies(&self, _dir: &Path, _table: &Table) -> std::io::Result<()> {
            Err(unsupported())
        }

        pub fn read(&self, _heard: &mut Vec<Heard>) -> std::io::Result<()> {
            Err(unsupported())
        }

        pub fn marks(&self) -> Vec<((u32, u32), u64)> {
            Vec::new()
        }

        pub fn forget(&self, _dev: (u32, u32)) {}

        pub fn resolver(&self, _table: &Table) -> Resolver {
            Resolver(())
        }
    }

    /// fanotify is Linux's.
    pub struct Resolver(());

    impl Resolver {
        pub fn resolve(&mut self, _folder: &Folder) -> Resolved {
            Resolved::Unmarked
        }
    }
}
