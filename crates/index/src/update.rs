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

use std::collections::BTreeMap;
use std::path::PathBuf;

use skwird_crawl::{
    Cancel, Crawled, Entry, Error, FileId, Kind, Options, Outcome, Root, Stale, Stored,
    StoredFolder, Time, Why, check, crawl_below, read_folder,
};

use crate::block::{Block, Facts, FolderId, Item, State};
use crate::build::{Top, blocks_of, facts};
use crate::index::{Index, component};
use crate::log::{Change, FolderRecord, Op, Record};
use crate::packed::Malformed;

/// What a read of a folder found, as [`Index::reread`] takes it: from a
/// crawl's listing, or rebuilt from a record of the log.
#[derive(Clone, Debug)]
pub enum Seen {
    Listed {
        again: bool,
        entries: Vec<(Vec<u8>, Entry)>,
    },
    NotCovered(Why),
}

impl Seen {
    pub fn of(outcome: &Outcome) -> Seen {
        match outcome {
            Outcome::Listed(listing) => Seen::Listed {
                again: listing.again,
                entries: listing.iter().map(|(n, e)| (n.to_vec(), *e)).collect(),
            },
            Outcome::NotCovered(why) => Seen::NotCovered(*why),
        }
    }
}

/// What a merge of a folder's entries found.
#[derive(Default)]
struct Merged {
    /// Folders whose subtrees go.
    gone: Vec<FolderId>,
    /// Folders new, their blocks not read yet.
    made: Vec<Block>,
    changes: Vec<Change>,
}

/// What a reread did.
#[derive(Clone, Debug, Default)]
pub struct Reread {
    /// The folders new in it, not read yet, to be crawled.
    pub fresh: Vec<FolderId>,
    /// What changed of its entries.
    pub changes: Vec<Change>,
}

/// The records of a crawl grafted in, each folder's entries all added,
/// a folder's before those below it (slots are given in that order).
pub fn records_of(crawl: &[Crawled]) -> Vec<Record> {
    crawl
        .iter()
        .map(|c| {
            let (state, changes) = match &c.outcome {
                Outcome::Listed(listing) => (
                    State::Listed {
                        again: listing.again,
                    },
                    listing
                        .iter()
                        .map(|(name, entry)| Change {
                            op: Op::Added,
                            name: name.to_vec(),
                            entry: *entry,
                        })
                        .collect(),
                ),
                Outcome::NotCovered(why) => (State::NotCovered(*why), Vec::new()),
            };
            Record::Folder(FolderRecord {
                path: c.rel.clone(),
                facts: c.stat.as_ref().map(facts),
                state,
                changes,
            })
        })
        .collect()
}

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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    /// What it changed, in the order it changed it, for the log.
    pub records: Vec<Record>,
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
    /// Every folder the check is to compare, and its number: each folder
    /// read, and each to be tried again. Excluded folders are not.
    ///
    /// No allocation is made for each folder (DESIGN 10): the paths go end
    /// to end into the list's buffer, and each is made in the buffer kept
    /// for its depth, which the next folder at that depth takes over.
    pub fn stored(&self) -> (Vec<FolderId>, Stored) {
        let folders = self.tree().len();
        let mut ids = Vec::with_capacity(folders);
        // About 64 bytes a path, grown in place if more.
        let mut out = Stored::with_capacity(folders, folders * 64);
        // The path of the last folder met at each depth.
        let mut at: Vec<PathBuf> = Vec::new();
        for (_, chunk) in self.tree().chunks() {
            let view = chunk.view();
            for b in 0..chunk.len() {
                let depth = view.depth(b) as usize;
                if at.len() <= depth {
                    at.resize_with(depth + 1, PathBuf::new);
                }
                let (above, here) = at.split_at_mut(depth);
                let path = &mut here[0];
                path.as_mut_os_string().clear();
                if let Some(parent) = above.last() {
                    path.push(parent);
                    path.push(component(self.names().get(view.name(b))));
                }
                let again = match view.state(b) {
                    State::Listed { again } => Some(again),
                    State::NotCovered(Why::Excluded) => None,
                    // Its `.private` taken away moves its change time.
                    State::NotCovered(Why::Private) => Some(false),
                    State::NotCovered(_) => Some(true),
                };
                if let Some(again) = again {
                    let facts = view.facts(b);
                    ids.push(view.folder(b));
                    out.push(
                        path,
                        StoredFolder {
                            id: facts.map(|f| f.id).unwrap_or_default(),
                            ctime: facts.map(|f| f.ctime).unwrap_or_default(),
                            again: again || facts.is_none(),
                        },
                    );
                }
            }
        }
        (ids, out)
    }

    /// Put what a read of `folder` found in place of what the index held.
    /// Gives the folders new in it, each a block not read yet, to be
    /// crawled ([`Index::graft`]), and what changed of its entries, for
    /// the log.
    ///
    /// Entries are matched by name. A folder that is still the same folder,
    /// by identity, keeps its block and what is below it; one gone, or
    /// another in its place, takes its subtree with it.
    pub fn reread(&mut self, folder: FolderId, facts: Option<Facts>, seen: Seen) -> Reread {
        let Some(old) = self.tree().block(folder) else {
            return Reread::default();
        };
        let mut m = Merged::default();
        let facts = facts.or(old.facts);
        let (state, items) = match seen {
            Seen::Listed { again, entries } => (
                State::Listed { again },
                self.merge(folder, &old, entries, &mut m),
            ),
            // Never kept: a private folder's names, a mount's. Their
            // removal is not logged entry by entry: a purge starts another
            // epoch of the log ([`crate::keeper`]).
            Seen::NotCovered(
                why @ (Why::Private | Why::Excluded | Why::Mount | Why::NetworkMount),
            ) => {
                m.gone.extend(old.items.iter().filter_map(|i| i.child));
                (State::NotCovered(why), Vec::new())
            }
            // Kept as it was read.
            Seen::NotCovered(why) => (State::NotCovered(why), old.items.clone()),
        };
        let fresh: Vec<FolderId> = m.made.iter().map(|b| b.folder).collect();
        self.tree_mut().replace(Block {
            facts,
            state,
            items,
            ..old
        });
        for child in m.gone {
            self.tree_mut().remove(child);
        }
        if !m.made.is_empty() {
            self.tree_mut().insert_after(folder, m.made);
        }
        Reread {
            fresh,
            changes: m.changes,
        }
    }

    /// The new entries of `folder`, matched by name with its block's
    /// `old` ones: what is gone, what is made and what changed, into `m`.
    fn merge(
        &mut self,
        folder: FolderId,
        old: &Block,
        mut entries: Vec<(Vec<u8>, Entry)>,
        m: &mut Merged,
    ) -> Vec<Item> {
        let dev = old.facts.map_or(0, |f| f.id.dev);
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut items = Vec::with_capacity(entries.len());
        let mut o = 0;
        for (name, entry) in &entries {
            let name = name.as_slice();
            // Old entries before this name are m.gone.
            while o < old.items.len() && self.names().get(old.items[o].name) < name {
                let item = old.items[o];
                m.gone.extend(item.child);
                m.changes.push(Change {
                    op: Op::Removed,
                    name: self.names().get(item.name).to_vec(),
                    entry: self.entry_of(&item, dev),
                });
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
                m.gone.push(old_child);
            }
            let child = match (entry.kind, kept) {
                (Kind::Folder, Some(child)) => Some(child),
                (Kind::Folder, None) => {
                    let id = self.tree_mut().alloc();
                    m.made.push(Block {
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
            let item = Item {
                name: name_id,
                kind: entry.kind,
                ino: entry.id.ino,
                attrs: entry.attrs,
                child,
            };
            match same {
                None => m.changes.push(Change {
                    op: Op::Added,
                    name: name.to_vec(),
                    entry: *entry,
                }),
                Some(was)
                    if was.kind != item.kind
                        || was.ino != item.ino
                        || was.attrs != item.attrs
                        || was.child != item.child =>
                {
                    m.changes.push(Change {
                        op: Op::Changed,
                        name: name.to_vec(),
                        entry: *entry,
                    });
                }
                Some(_) => {}
            }
            items.push(item);
        }
        for item in &old.items[o..] {
            m.gone.extend(item.child);
            m.changes.push(Change {
                op: Op::Removed,
                name: self.names().get(item.name).to_vec(),
                entry: self.entry_of(item, dev),
            });
        }
        items
    }

    /// An entry of a block, as the log and the journal give it: a folder's
    /// device is its own block's.
    fn entry_of(&self, item: &Item, dev: u64) -> Entry {
        let dev = item
            .child
            .and_then(|c| self.facts_of(c))
            .map_or(dev, |f| f.id.dev);
        Entry {
            kind: item.kind,
            id: FileId { dev, ino: item.ino },
            attrs: item.attrs,
        }
    }

    /// Take a record of the log again, as it was taken when it was made.
    /// A record about a folder the index does not hold is an error: the
    /// log and the chunks do not belong together.
    pub fn replay(&mut self, record: &FolderRecord) -> Result<(), Malformed> {
        let folder = self.folder_at(&record.path).ok_or(Malformed)?;
        let seen = match record.state {
            State::Listed { again } => {
                let old = self.tree().block(folder).ok_or(Malformed)?;
                let dev = old.facts.map_or(0, |f| f.id.dev);
                let mut entries: BTreeMap<Vec<u8>, Entry> = old
                    .items
                    .iter()
                    .map(|item| {
                        (
                            self.names().get(item.name).to_vec(),
                            self.entry_of(item, dev),
                        )
                    })
                    .collect();
                for change in &record.changes {
                    match change.op {
                        Op::Removed => {
                            entries.remove(&change.name);
                        }
                        Op::Added | Op::Changed => {
                            entries.insert(change.name.clone(), change.entry);
                        }
                    }
                }
                Seen::Listed {
                    again,
                    entries: entries.into_iter().collect(),
                }
            }
            State::NotCovered(why) => Seen::NotCovered(why),
        };
        self.reread(folder, record.facts, seen);
        Ok(())
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
    let (ids, stored) = index.stored();
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
        let (found, seen) = match stale {
            Stale::Mount => (None, Seen::NotCovered(Why::Mount)),
            Stale::NetworkMount => (None, Seen::NotCovered(Why::NetworkMount)),
            _ => {
                let (found, outcome) = read_folder(root, stored.rel(i), opts);
                (found, Seen::of(&outcome))
            }
        };
        let folder_facts = found.as_ref().map(facts);
        let state = match &seen {
            Seen::Listed { again, .. } => State::Listed { again: *again },
            Seen::NotCovered(why) => State::NotCovered(*why),
        };
        let reread = index.reread(folder, folder_facts, seen);
        fresh.extend(reread.fresh);
        done.records.push(Record::Folder(FolderRecord {
            path: stored.rel(i).to_path_buf(),
            facts: folder_facts.or_else(|| index.facts_of(folder)),
            state,
            changes: reread.changes,
        }));
        done.reread += 1;
    }
    while let Some(folder) = fresh.pop() {
        if index.tree().loc(folder).is_none() {
            continue;
        }
        let rel = index.path_of(folder);
        let crawl = crawl_below(root, &rel, opts, cancel)?;
        done.crawled += crawl.folders.len();
        done.records.extend(records_of(&crawl.folders));
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
