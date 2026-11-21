//! An index made from a crawl: each folder's block in pre-order, its
//! entries in the order of their names' bytes, its folders' blocks after
//! it in that order, every name stored once.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use skwird_crawl::{
    Cancel, Crawl, Crawled, Error, FolderStat, Kind, Options, Outcome, Root, Watch, crawl,
};

use crate::block::{Block, Facts, FolderId, Item, NameId, State};
use crate::index::Index;
use crate::packer::Packer;
use crate::tree::Limits;

pub fn facts(stat: &FolderStat) -> Facts {
    Facts {
        id: stat.id,
        ctime: stat.ctime,
        mtime: stat.mtime,
        owner: stat.owner,
        group: stat.group,
        mode: stat.mode,
    }
}

/// The last name of a folder's path below the root.
fn last_name(crawled: &Crawled) -> &[u8] {
    crawled
        .rel
        .file_name()
        .map_or(&[], std::ffi::OsStr::as_encoded_bytes)
}

/// Where the top of a crawled subtree goes in the index.
#[derive(Clone, Copy, Debug)]
pub struct Top {
    pub parent: Option<FolderId>,
    pub name: NameId,
    pub depth: u32,
}

/// The blocks of a crawled subtree, in pre-order: a folder, then each of
/// its folders' subtrees in the order of their names. `ids` gives each
/// slot's folder number (slot 0 is the subtree's top), `intern` each
/// name's. Each folder's listing is let go of once its block is made, so
/// the crawl and the blocks are not both held whole.
pub fn blocks_of(
    crawl: Vec<Crawled>,
    top: Top,
    ids: &mut dyn FnMut(usize) -> FolderId,
    intern: &mut dyn FnMut(&[u8]) -> NameId,
) -> Vec<Block> {
    let mut folders = crawl;
    let id_of: Vec<FolderId> = (0..folders.len()).map(&mut *ids).collect();
    // Each folder's folders, by slot.
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); folders.len()];
    for (slot, f) in folders.iter().enumerate() {
        if let Some(parent) = f.parent {
            kids[parent].push(slot);
        }
    }
    let mut blocks = Vec::with_capacity(folders.len());
    let mut stack: Vec<(usize, u32)> = vec![(0, top.depth)];
    while let Some((slot, depth)) = stack.pop() {
        let outcome = std::mem::replace(
            &mut folders[slot].outcome,
            Outcome::NotCovered(skwird_crawl::Why::Unread),
        );
        let by_name: HashMap<&[u8], usize> = kids[slot]
            .iter()
            .map(|&c| (last_name(&folders[c]), c))
            .collect();
        let mut items = Vec::new();
        let mut below = Vec::new();
        let state = match outcome {
            Outcome::Listed(listing) => {
                let mut entries: Vec<(&[u8], &skwird_crawl::Entry)> = listing.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                for (name, entry) in entries {
                    let child = (entry.kind == Kind::Folder)
                        .then(|| by_name.get(name).copied())
                        .flatten();
                    below.extend(child);
                    items.push(Item {
                        name: intern(name),
                        kind: entry.kind,
                        ino: entry.id.ino,
                        attrs: entry.attrs,
                        child: child.map(|c| id_of[c]),
                    });
                }
                State::Listed {
                    again: listing.again,
                }
            }
            Outcome::NotCovered(why) => State::NotCovered(why),
        };
        for &child in below.iter().rev() {
            stack.push((child, depth + 1));
        }
        let f = &folders[slot];
        let (parent, name) = if slot == 0 {
            (top.parent, top.name)
        } else {
            let parent = f.parent.expect("a folder below the top has a parent");
            (Some(id_of[parent]), intern(last_name(f)))
        };
        blocks.push(Block {
            folder: id_of[slot],
            parent,
            name,
            depth,
            facts: f.stat.as_ref().map(facts),
            state,
            items,
        });
    }
    blocks
}

/// The index of what `crawl` read, the root's folder first (slot 0), each
/// folder's listing let go of once packed ([`Packer`]).
pub fn build(crawl: Crawl, limits: Limits) -> Index {
    let mut packer = Packer::default();
    for (slot, crawled) in crawl.folders.into_iter().enumerate() {
        packer.add(slot, &crawled);
    }
    packer.finish(limits)
}

/// The index of the whole root, crawled now, with how long the crawl took
/// and what came of watching each folder ([`Options::watcher`]).
pub struct Built {
    pub index: Index,
    pub took: Duration,
    pub watches: Vec<(FolderId, Watch)>,
}

/// The index of the whole root, crawled now: each folder packed as the
/// crawl reads it, on a thread of its own, so that the crawl is never
/// held whole (`docs/PLAN.md`, phase 1, step 7).
pub fn crawl_and_build(
    root: &Root,
    opts: &Options,
    cancel: &Cancel,
    limits: Limits,
) -> Result<Built, Error> {
    // A few folders ahead: the crawl waits for the packer rather than hold
    // what it has not packed yet.
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, Crawled)>(256);
    std::thread::scope(|scope| {
        let packing = scope.spawn(move || {
            let mut packer = Packer::default();
            for (slot, crawled) in rx {
                packer.add(slot, &crawled);
            }
            packer
        });
        let took = crawl(root, Path::new(""), opts, cancel, &|slot, crawled| {
            // The packer ends only once every sender is gone.
            let _ = tx.send((slot, crawled));
        });
        drop(tx);
        let packer = packing.join().expect("the packer does not panic");
        took.map(|took| {
            let (index, watches) = packer.finish_watched(limits);
            Built {
                index,
                took,
                watches,
            }
        })
    })
}
