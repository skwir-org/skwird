//! What a folder holds, from the index (`docs/DESIGN.md`, Protocol,
//! `totals`): each child's totals and the folder's own, summed over the
//! subtree when asked, never kept as running aggregates, so that they are
//! as current as the index.
//!
//! - **Files** are every entry but folders: their sizes and allocs are
//!   summed, by extension too; a folder's own bytes are not.
//! - **A file of several names** (a count of names above one) is counted
//!   once in a subtree, by its identity, and is `shared` where it has more
//!   names than the subtree holds. Only those are held apart while a
//!   subtree is summed: they are few.
//! - **A count not known** (0: Windows' listings say none) is taken as
//!   one name: such a file is counted at each of its names, and the
//!   answer says its counts are partial, `shared` at least what it says.
//!   Holding every such file apart would hold all of them.
//! - **What is not counted:** a folder not covered, or one the asker may
//!   not list (its sight hidden, or not known), is a hole, and nothing
//!   below it counts, so that no total says what it holds.

use std::collections::HashMap;

use skwird_crawl::{FileId, Kind, Time, Why};

use crate::block::{FolderId, Item, State};
use crate::index::{Index, Sight, SightOf};

/// A subtree's sums.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sum {
    pub size: u64,
    pub alloc: u64,
    /// Every entry but folders, a file of several names once.
    pub files: u64,
    /// Folders below, the subtree's own not counted.
    pub folders: u64,
    /// The newest modification time of its files: none where it holds
    /// none. A folder's own is beside it ([`Child::mtime`]), for a client
    /// to date a folder holding no file by it.
    pub newest: Option<Time>,
    /// Files and alloc by extension ([`extension`]).
    pub by_extension: HashMap<Option<Vec<u8>>, (u64, u64)>,
    /// The alloc of its files with more names than it holds.
    pub shared: u64,
}

impl Sum {
    fn newer(&mut self, t: Time) {
        if self.newest.is_none_or(|n| t > n) {
            self.newest = Some(t);
        }
    }

    fn file(&mut self, size: u64, alloc: u64, ext: Option<Vec<u8>>, mtime: Option<Time>) {
        self.files += 1;
        self.size += size;
        self.alloc += alloc;
        let e = self.by_extension.entry(ext).or_default();
        e.0 += 1;
        e.1 += alloc;
        if let Some(t) = mtime {
            self.newer(t);
        }
    }

    fn add(&mut self, other: &Sum) {
        self.size += other.size;
        self.alloc += other.alloc;
        self.files += other.files;
        self.folders += other.folders;
        self.shared += other.shared;
        if let Some(t) = other.newest {
            self.newer(t);
        }
        for (ext, (files, alloc)) in &other.by_extension {
            let e = self.by_extension.entry(ext.clone()).or_default();
            e.0 += files;
            e.1 += alloc;
        }
    }
}

/// A file of several names, as a subtree holds it.
#[derive(Clone)]
struct Multi {
    size: u64,
    alloc: u64,
    links: u32,
    /// Its names in the subtree.
    names: u32,
    ext: Option<Vec<u8>>,
    mtime: Option<Time>,
}

/// A subtree as it is summed: what counts plainly, and its files of
/// several names, each counted once at the end.
#[derive(Default)]
struct Part {
    sum: Sum,
    multi: HashMap<FileId, Multi>,
}

impl Part {
    /// What it sums to, each file of several names once, shared where it
    /// has names outside.
    fn total(&self) -> Sum {
        let mut sum = self.sum.clone();
        for m in self.multi.values() {
            sum.file(m.size, m.alloc, m.ext.clone(), m.mtime);
            if m.links > m.names {
                sum.shared += m.alloc;
            }
        }
        sum
    }

    /// `other`'s subtree taken into this one: a file of several names in
    /// both counted once, its names in both.
    fn absorb(&mut self, other: &Part) {
        self.sum.add(&other.sum);
        for (id, m) in &other.multi {
            self.multi
                .entry(*id)
                .and_modify(|held| held.names += m.names)
                .or_insert_with(|| m.clone());
        }
    }
}

/// A child of the folder asked, with its totals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    /// Its name in the folder.
    pub name: Vec<u8>,
    pub kind: Kind,
    pub id: FileId,
    /// Its own modification time, a folder's whatever it holds: none where
    /// the index holds no attributes for it.
    pub mtime: Option<Time>,
    pub sum: Sum,
}

/// What a folder holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// The folder's own, every child counted.
    pub own: Sum,
    /// The folder's own modification time.
    pub mtime: Option<Time>,
    pub children: Vec<Child>,
    /// The folders not counted, and why: not covered, or not seen by the
    /// asker ([`Why::Denied`]).
    pub holes: Vec<(FolderId, Why)>,
    /// A file whose count of names is not known was counted.
    pub partial: bool,
}

/// A name's extension: from its last dot on, the dot left out, lowercased
/// (as UTF-8 where it is, its ASCII alone otherwise); none for a name with
/// no dot past its first byte (`.profile`), or nothing after it.
pub fn extension(name: &[u8]) -> Option<Vec<u8>> {
    let dot = name.iter().rposition(|&b| b == b'.').filter(|&d| d > 0)?;
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        return None;
    }
    Some(match std::str::from_utf8(ext) {
        Ok(text) => text.to_lowercase().into_bytes(),
        Err(_) => ext.to_ascii_lowercase(),
    })
}

/// Who sees what: the asker's sight of a folder, remembered.
struct Seer<'a> {
    sight: Option<SightOf<'a>>,
    memo: HashMap<FolderId, Sight>,
}

impl Seer<'_> {
    fn sees(&mut self, folder: FolderId) -> bool {
        self.sight
            .is_none_or(|s| s(folder, &mut self.memo) == Sight::Seen)
    }
}

impl Index {
    /// What `folder` holds ([`Totals`]): each of its children with its
    /// totals, in the order of its entries, and its own; for an asker
    /// (`sight`, as [`Index::query_seen`] takes it), what they may list
    /// alone. None if it is not a folder of the index.
    pub fn totals(&self, folder: FolderId, sight: Option<SightOf<'_>>) -> Option<Totals> {
        let block = self.tree().block(folder)?;
        let mut seer = Seer {
            sight,
            memo: HashMap::new(),
        };
        let mut totals = Totals {
            mtime: self.facts_of(folder).map(|f| f.mtime),
            ..Totals::default()
        };
        if let State::NotCovered(why) = block.state {
            totals.holes.push((folder, why));
            return Some(totals);
        }
        let dev = self.facts_of(folder).map(|f| f.id.dev).unwrap_or_default();
        let mut whole = Part::default();
        for item in &block.items {
            let name = self.names().get(item.name).to_vec();
            let mut part = Part::default();
            let id = match (item.kind, item.child) {
                (Kind::Folder, Some(child)) => {
                    self.sum_below(child, &mut seer, &mut part, &mut totals);
                    whole.sum.folders += 1;
                    let child_dev = self.facts_of(child).map_or(dev, |f| f.id.dev);
                    FileId {
                        dev: child_dev,
                        ino: item.ino,
                    }
                }
                (Kind::Folder, None) => {
                    whole.sum.folders += 1;
                    FileId { dev, ino: item.ino }
                }
                _ => {
                    let id = FileId { dev, ino: item.ino };
                    add_file(&mut part, item, &name, id, &mut totals.partial);
                    id
                }
            };
            whole.absorb(&part);
            totals.children.push(Child {
                name,
                kind: item.kind,
                id,
                mtime: item.attrs.map(|a| a.mtime),
                sum: part.total(),
            });
        }
        totals.own = whole.total();
        Some(totals)
    }

    /// The subtree of `top` summed into `part`, in its pre-order: a folder
    /// not covered, or not seen, a hole in `totals`, nothing below it read.
    fn sum_below(&self, top: FolderId, seer: &mut Seer<'_>, part: &mut Part, totals: &mut Totals) {
        // Below a folder passed over, every block deeper than it.
        let mut passed: Option<u32> = None;
        for (chunk, blocks) in self.tree().ranges(top) {
            let view = self.tree().chunk(chunk).view();
            for b in blocks {
                let depth = view.depth(b);
                if passed.is_some_and(|d| depth > d) {
                    continue;
                }
                passed = None;
                let folder = view.folder(b);
                if let State::NotCovered(why) = view.state(b) {
                    totals.holes.push((folder, why));
                    passed = Some(depth);
                    continue;
                }
                if !seer.sees(folder) {
                    totals.holes.push((folder, Why::Denied));
                    passed = Some(depth);
                    continue;
                }
                let dev = view.facts(b).map(|f| f.id.dev).unwrap_or_default();
                for item in view.items(view.entries(b)) {
                    if item.kind == Kind::Folder {
                        part.sum.folders += 1;
                        continue;
                    }
                    let name = self.names().get(item.name);
                    let id = FileId { dev, ino: item.ino };
                    add_file(part, &item, name, id, &mut totals.partial);
                }
            }
        }
    }
}

/// A file (any entry but a folder) of `part`'s: counted plainly where it
/// has one name, or its count is not known (`partial` said then); held
/// apart, by its identity, where it has several.
fn add_file(part: &mut Part, item: &Item, name: &[u8], id: FileId, partial: &mut bool) {
    let ext = extension(name);
    let Some(a) = item.attrs else {
        part.sum.file(0, 0, ext, None);
        return;
    };
    if a.links <= 1 {
        *partial |= a.links == 0;
        part.sum.file(a.size, a.alloc, ext, Some(a.mtime));
        return;
    }
    part.multi
        .entry(id)
        .and_modify(|m| m.names += 1)
        .or_insert(Multi {
            size: a.size,
            alloc: a.alloc,
            links: a.links,
            names: 1,
            ext,
            mtime: Some(a.mtime),
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An extension is the name's last dot on, lowercased; a name led by
    /// its one dot, or ending in one, has none.
    #[test]
    fn an_extension_is_the_last_dots_on_lowercased() {
        assert_eq!(extension(b"Photo.JPG"), Some(b"jpg".to_vec()));
        assert_eq!(extension(b"archive.tar.GZ"), Some(b"gz".to_vec()));
        assert_eq!(
            extension("Été.ÉTÉ".as_bytes()),
            Some("été".as_bytes().to_vec())
        );
        assert_eq!(extension(b".profile"), None);
        assert_eq!(extension(b"README"), None);
        assert_eq!(extension(b"trailing."), None);
        assert_eq!(extension(b"bytes.\xffAB"), Some(b"\xffab".to_vec()));
    }
}
