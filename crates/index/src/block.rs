//! A folder block: a folder's own facts and its entries, the unit the
//! index lays out depth first (`docs/DESIGN.md`, decision 8).

use skwird_crawl::{Attrs, FileId, Kind, Time, Why};

/// A folder's number in the index. It stays the folder's while the
/// folder is in the index, whatever chunk holds its block; a number let
/// go of may be given to a folder made later.
pub type FolderId = u32;

/// A name's number in the index's names ([`crate::names`]).
pub type NameId = u32;

/// What the index knows of a folder's entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Read; `again` if it must be read again at the next check whatever
    /// its times say.
    Listed { again: bool },
    /// Not covered, and why. What the block holds stays as of its last
    /// read (a failed read is not an empty folder).
    NotCovered(Why),
}

/// A folder's own facts, from its stat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub id: FileId,
    pub ctime: Time,
    pub mtime: Time,
    pub owner: u32,
    pub group: u32,
    pub mode: u16,
}

/// An entry of a folder, as the index keeps it. Its device is its
/// folder's, but for a folder whose own block says another (a btrfs
/// subvolume's top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub name: NameId,
    pub kind: Kind,
    pub ino: u64,
    pub attrs: Option<Attrs>,
    /// The folder's own block, for an entry that is a folder.
    pub child: Option<FolderId>,
}

/// A folder's block: its place in the tree, its facts, what is known of
/// its entries, and its entries, in the order of their names' bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub folder: FolderId,
    /// The folder holding it: none for the index's root.
    pub parent: Option<FolderId>,
    /// Its name in its parent (the root's is empty).
    pub name: NameId,
    /// How many folders are above it, the root's being 0.
    pub depth: u32,
    /// Its facts, where a stat gave them: none for a mount point, never
    /// stat-ed.
    pub facts: Option<Facts>,
    pub state: State,
    pub items: Vec<Item>,
}
