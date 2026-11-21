//! An index kept current, and what it can claim (`docs/DESIGN.md`,
//! decision 2): coverage, never overstated.
//!
//! - [`refresh`] proves the index current where nothing watched: the
//!   check finds the folders to read again, each is read and put in place
//!   of what the index held ([`Index::reread`]), and each folder new since
//!   is crawled ([`Index::graft`]). Done, the index's names are checked as
//!   of the time it began.
//! - [`Index::coverage`] says, for a folder, how far its subtree can be
//!   trusted: the proof of its names, the time its attributes are as of,
//!   and each subtree below it that is not covered, with why.
//!
//! What is not read stays as it was read, and is said not covered: a
//! failed read is not an empty folder. But what is private, excluded or
//! under a mount is let go of at once: names of a private folder are never
//! kept, and a mount's names are not the folder's.

use std::path::PathBuf;

use skwird_crawl::{
    Cancel, Entry, Error, FolderStat, Kind, Options, Outcome, Root, Stale, Stored, Time, Why,
    check, crawl_below, read_folder,
};

use crate::block::{Block, FolderId, Item, State};
use crate::build::{Top, blocks_of, facts};
use crate::index::{Index, component};

/// How far an index's names can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    /// Every folder was compared with the index at this time, or read
    /// after it.
    Checked(Time),
    /// A snapshot of this time, not proven since: an index just loaded.
    AsOf(Time),
}

/// A subtree whose entries are not covered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hole {
    /// Its folder's path below the index's root.
    pub path: PathBuf,
    pub why: Why,
}

/// How far an answer below a folder can be trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// Names and kinds, below the folder but in its holes.
    pub names: Proof,
    /// Sizes and times are as of this time at least: the check proves
    /// names, and an edit inside a file moves no folder's time.
    pub attrs: Time,
    /// The topmost folders below it that are not covered, and why: what
    /// is under each is not covered either.
    pub holes: Vec<Hole>,
}

/// What a refresh did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    /// Folders the check compared.
    pub checked: usize,
    /// Folders read again.
    pub reread: usize,
    /// Folders new since, crawled.
    pub crawled: usize,
}

/// Whether `entry` is the folder whose block is `child`, by identity.
fn same_folder(index: &Index, child: FolderId, item: &Item, entry: &Entry) -> bool {
    match index.facts_of(child) {
        Some(facts) => facts.id == entry.id,
        // Never stat-ed (a mount point, a folder not read yet): by the
        // inode its parent's listing gives.
        None => item.ino == entry.id.ino,
    }
}

impl Index {
    /// Every folder the check is to compare, with its number: each folder
    /// read, and each to be tried again. Excluded folders are not.
    pub fn stored(&self) -> Vec<(FolderId, Stored)> {
        let mut out = Vec::new();
        // The paths of the folders above, by depth.
        let mut above: Vec<PathBuf> = Vec::new();
        for (_, chunk) in self.tree().chunks() {
            let view = chunk.view();
            for b in 0..chunk.len() {
                let depth = view.depth(b) as usize;
                above.truncate(depth);
                let path = match above.last() {
                    Some(parent) => parent.join(component(self.names().get(view.name(b)))),
                    None => PathBuf::new(),
                };
                let again = match view.state(b) {
                    State::Listed { again } => Some(again),
                    State::NotCovered(Why::Excluded) => None,
                    // Its `.private` taken away moves its change time.
                    State::NotCovered(Why::Private) => Some(false),
                    State::NotCovered(_) => Some(true),
                };
                if let Some(again) = again {
                    let facts = view.facts(b);
                    out.push((
                        view.folder(b),
                        Stored {
                            rel: path.clone(),
                            id: facts.map(|f| f.id).unwrap_or_default(),
                            ctime: facts.map(|f| f.ctime).unwrap_or_default(),
                            again: again || facts.is_none(),
                        },
                    ));
                }
                above.push(path);
            }
        }
        out
    }

    /// Put what a read of `folder` found in place of what the index held,
    /// and give the folders new in it, each a block not read yet, to be
    /// crawled ([`Index::graft`]).
    ///
    /// Entries are matched by name. A folder that is still the same folder,
    /// by identity, keeps its block and what is below it; one gone, or
    /// another in its place, takes its subtree with it.
    pub fn reread(
        &mut self,
        folder: FolderId,
        stat: Option<FolderStat>,
        outcome: Outcome,
    ) -> Vec<FolderId> {
        let Some(old) = self.tree().block(folder) else {
            return Vec::new();
        };
        let mut gone: Vec<FolderId> = Vec::new();
        let mut made: Vec<Block> = Vec::new();
        let facts = stat.as_ref().map(facts).or(old.facts);
        let (state, items) = match outcome {
            Outcome::Listed(listing) => {
                let mut entries: Vec<(&[u8], &Entry)> = listing.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                let mut items = Vec::with_capacity(entries.len());
                let mut o = 0;
                for (name, entry) in entries {
                    // Old entries before this name are gone.
                    while o < old.items.len() && self.names().get(old.items[o].name) < name {
                        gone.extend(old.items[o].child);
                        o += 1;
                    }
                    let same = old
                        .items
                        .get(o)
                        .filter(|item| self.names().get(item.name) == name)
                        .copied();
                    if same.is_some() {
                        o += 1;
                    }
                    let name_id = match same {
                        Some(item) => item.name,
                        None => self.names_mut().intern(name),
                    };
                    let kept = same.and_then(|item| {
                        let child = item.child?;
                        (entry.kind == Kind::Folder && same_folder(self, child, &item, entry))
                            .then_some(child)
                    });
                    if let Some(old_child) = same.and_then(|item| item.child)
                        && kept != Some(old_child)
                    {
                        gone.push(old_child);
                    }
                    let child = match (entry.kind, kept) {
                        (Kind::Folder, Some(child)) => Some(child),
                        (Kind::Folder, None) => {
                            let id = self.tree_mut().alloc();
                            made.push(Block {
                                folder: id,
                                parent: Some(folder),
                                name: name_id,
                                depth: old.depth + 1,
                                facts: None,
                                state: State::NotCovered(Why::Unread),
                                items: Vec::new(),
                            });
                            Some(id)
                        }
                        _ => None,
                    };
                    items.push(Item {
                        name: name_id,
                        kind: entry.kind,
                        ino: entry.id.ino,
                        attrs: entry.attrs,
                        child,
                    });
                }
                for item in &old.items[o..] {
                    gone.extend(item.child);
                }
                (
                    State::Listed {
                        again: listing.again,
                    },
                    items,
                )
            }
            // Never kept: a private folder's names, a mount's.
            Outcome::NotCovered(
                why @ (Why::Private | Why::Excluded | Why::Mount | Why::NetworkMount),
            ) => {
                gone.extend(old.items.iter().filter_map(|i| i.child));
                (State::NotCovered(why), Vec::new())
            }
            // Kept as it was read.
            Outcome::NotCovered(why) => (State::NotCovered(why), old.items.clone()),
        };
        let fresh: Vec<FolderId> = made.iter().map(|b| b.folder).collect();
        self.tree_mut().replace(Block {
            facts,
            state,
            items,
            ..old
        });
        for child in gone {
            self.tree_mut().remove(child);
        }
        if !made.is_empty() {
            self.tree_mut().insert_after(folder, made);
        }
        fresh
    }

    /// Put a crawl of `folder`'s subtree in place of its block: slot 0 is
    /// the folder itself, every other slot a folder below it.
    pub fn graft(&mut self, folder: FolderId, crawl: Vec<skwird_crawl::Crawled>) {
        let Some(old) = self.tree().block(folder) else {
            return;
        };
        for child in old.items.iter().filter_map(|i| i.child) {
            self.tree_mut().remove(child);
        }
        let top = Top {
            parent: old.parent,
            name: old.name,
            depth: old.depth,
        };
        let (tree, names) = self.parts_mut();
        let mut blocks = blocks_of(
            crawl,
            top,
            &mut |slot| if slot == 0 { folder } else { tree.alloc() },
            &mut |name| names.intern(name),
        )
        .into_iter();
        if let Some(first) = blocks.next() {
            tree.replace(first);
        }
        let rest: Vec<Block> = blocks.collect();
        if !rest.is_empty() {
            tree.insert_after(folder, rest);
        }
    }

    /// How far an answer below `folder` can be trusted.
    pub fn coverage(&self, folder: FolderId) -> Coverage {
        let mut holes = Vec::new();
        // The depth of the hole being passed through, if any.
        let mut inside: Option<u32> = None;
        for (chunk, blocks) in self.tree().ranges(folder) {
            let view = self.tree().chunk(chunk).view();
            for b in blocks {
                let depth = view.depth(b);
                if inside.is_some_and(|d| depth > d) {
                    continue;
                }
                inside = None;
                if let State::NotCovered(why) = view.state(b) {
                    holes.push(Hole {
                        path: self.path_of(view.folder(b)),
                        why,
                    });
                    inside = Some(depth);
                }
            }
        }
        Coverage {
            names: self.proof(),
            attrs: self.crawled_at(),
            holes,
        }
    }
}

/// Prove `index` current below `root`, where nothing watched: the mount
/// table read again, every stored folder checked, each stale one read
/// again, each new one crawled. Done, its names are checked as of when
/// this began; on a filesystem whose change times do not tell (FUSE),
/// as of then alone. Cancelled, the index stays sound, its proof as it
/// was. A root replaced since it was opened is an error: open it again,
/// and build again.
pub fn refresh(
    index: &mut Index,
    root: &mut Root,
    opts: &Options,
    cancel: &Cancel,
) -> Result<Refreshed, Error> {
    let start = Time::now();
    root.reread_mounts().map_err(Error::Root)?;
    let (ids, stored): (Vec<FolderId>, Vec<Stored>) = index.stored().into_iter().unzip();
    let checked = check(root, &stored, opts, cancel)?;
    let mut done = Refreshed {
        checked: stored.len(),
        ..Refreshed::default()
    };
    let top = index.root();
    let mut fresh = Vec::new();
    for (i, stale) in checked.stale {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let folder = ids[i];
        // Gone with a folder above it, read again before it.
        if index.tree().loc(folder).is_none() {
            continue;
        }
        if Some(folder) == top && matches!(stale, Stale::Gone | Stale::Replaced) {
            return Err(Error::Root(std::io::Error::other(
                "the root was replaced since it was opened",
            )));
        }
        let (seen, outcome) = match stale {
            Stale::Mount => (None, Outcome::NotCovered(Why::Mount)),
            Stale::NetworkMount => (None, Outcome::NotCovered(Why::NetworkMount)),
            _ => read_folder(root, &stored[i].rel, opts),
        };
        fresh.extend(index.reread(folder, seen, outcome));
        done.reread += 1;
    }
    while let Some(folder) = fresh.pop() {
        if index.tree().loc(folder).is_none() {
            continue;
        }
        let rel = index.path_of(folder);
        let crawl = crawl_below(root, &rel, opts, cancel)?;
        done.crawled += crawl.folders.len();
        index.graft(folder, crawl.folders);
    }
    let tells = root.fs_name() != "fuse";
    index.set_proof(if tells {
        Proof::Checked(start)
    } else {
        Proof::AsOf(start)
    });
    Ok(done)
}
