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

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use std::path::{Path, PathBuf};

use skwird_crawl::{
    Cancel, Crawled, Entry, Error, FileId, Kind, Options, Outcome, Root, Stale, Stored,
    StoredFolder, Time, Watch, Why, check, check_watched, crawl_below, read_folder_reusing,
    read_folder_watched,
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
    Listed { again: bool, entries: Entries },
    NotCovered(Why),
}

impl Seen {
    /// What a crawl's read found, its listing taken as it is: no name
    /// copied (DESIGN 8).
    pub fn of(outcome: Outcome) -> Seen {
        match outcome {
            Outcome::Listed(listing) => Seen::Listed {
                again: listing.again,
                entries: Entries::Listing(listing),
            },
            Outcome::NotCovered(why) => Seen::NotCovered(why),
        }
    }
}

/// A folder's entries read, for a merge: a crawl's listing as it is, its
/// names in one buffer, or entries one by one, as a record replayed gives
/// them.
#[derive(Clone, Debug)]
pub enum Entries {
    Listing(skwird_crawl::Listing),
    Each(Vec<(Vec<u8>, Entry)>),
}

impl Entries {
    pub fn len(&self) -> usize {
        match self {
            Entries::Listing(listing) => listing.len(),
            Entries::Each(each) => each.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// In the order of the names' bytes, as a merge walks them: a listing
    /// a crawl sorted as it is, once checked to be; any other sorted here.
    fn in_order(self) -> Entries {
        let by_name = |a: &(Vec<u8>, Entry), b: &(Vec<u8>, Entry)| a.0.cmp(&b.0);
        match self {
            Entries::Listing(listing) if listing.is_sorted() => Entries::Listing(listing),
            Entries::Listing(listing) => {
                let mut each: Vec<_> = listing.iter().map(|(n, e)| (n.to_vec(), *e)).collect();
                each.sort_unstable_by(by_name);
                Entries::Each(each)
            }
            Entries::Each(mut each) => {
                each.sort_unstable_by(by_name);
                Entries::Each(each)
            }
        }
    }

    /// Each entry with its name, in the order it holds them.
    fn by_name(&self) -> Box<dyn Iterator<Item = (&[u8], &Entry)> + '_> {
        match self {
            Entries::Listing(listing) => Box::new(listing.iter()),
            Entries::Each(each) => Box::new(each.iter().map(|(n, e)| (n.as_slice(), e))),
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
    /// How its block was written: what its read cost the tree.
    pub written: Written,
}

/// How a folder's block read again was written into the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Written {
    /// Not: it read back as it was.
    #[default]
    Not,
    /// Patched into its chunk, its shape the same (a file written to).
    Patched,
    /// Spliced into its chunk's columns, its entries changed (an entry
    /// made or removed), the chunk left as a cut would leave it.
    Spliced,
    /// Its chunk cut and encoded again (an entry made or removed, a
    /// folder made, moved or let go of).
    Cut,
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
    /// Entries the folders read again held, each stat-ed: what a read
    /// costs; and the most one folder held.
    pub entries: usize,
    pub largest: usize,
    /// The folders read again, in order, each with the entries its
    /// listing held: what its read cost.
    pub folders: Vec<(FolderId, usize)>,
    /// What came of watching each folder read or crawled, where a watch
    /// was asked ([`Options::watcher`]).
    pub watches: Vec<(FolderId, Watch)>,
    /// Of the folders read again, how many were patched into their chunk,
    /// how many spliced into it, and how many cut it again ([`Written`]).
    pub patched: usize,
    pub spliced: usize,
    pub cut: usize,
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
    /// Every folder the check is to compare whose path `keep` keeps, and
    /// its number: each folder
    /// read, and each to be tried again. Excluded folders are not.
    ///
    /// No allocation is made for each folder (DESIGN 10): the paths go end
    /// to end into the list's buffer, and each is made in the buffer kept
    /// for its depth, which the next folder at that depth takes over.
    pub fn stored(&self, keep: impl Fn(&Path) -> bool) -> (Vec<FolderId>, Stored) {
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
                if let Some(again) = again.filter(|_| keep(path)) {
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
        match self.tree().block(folder) {
            Some(old) => self.reread_block(&old, facts, seen),
            None => Reread::default(),
        }
    }

    /// [`Index::reread`], of `old`, its folder's block as the tree holds it
    /// now, decoded once by the caller.
    pub fn reread_block(&mut self, old: &Block, facts: Option<Facts>, seen: Seen) -> Reread {
        let folder = old.folder;
        let mut m = Merged::default();
        let facts = facts.or(old.facts);
        let (state, items) = match seen {
            Seen::Listed { again, entries } => (
                State::Listed { again },
                self.merge(folder, old, entries, &mut m),
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
        // A block as it was is not written again: a chunk rewritten costs
        // its whole, whatever changed (DESIGN 8), and many a folder heard
        // reads back as it was.
        let block = Block {
            folder: old.folder,
            parent: old.parent,
            name: old.name,
            depth: old.depth,
            facts,
            state,
            items,
        };
        let mut written = Written::Not;
        if block != *old {
            // A file written to, its folder the same shape: its chunk
            // patched. An entry made or removed: its chunk's columns
            // spliced. Else its chunk cut again.
            written = if self.tree_mut().patch(old, &block) {
                Written::Patched
            } else if self.tree_mut().splice(old, &block) {
                Written::Spliced
            } else {
                self.tree_mut().replace(block);
                Written::Cut
            };
        }
        for child in m.gone {
            self.tree_mut().remove(child);
        }
        if !m.made.is_empty() {
            self.tree_mut().insert_after(folder, m.made);
        }
        Reread {
            fresh,
            changes: m.changes,
            written,
        }
    }

    /// The new entries of `folder`, matched by name with its block's
    /// `old` ones: what is gone, what is made and what changed, into `m`.
    fn merge(
        &mut self,
        folder: FolderId,
        old: &Block,
        entries: Entries,
        m: &mut Merged,
    ) -> Vec<Item> {
        let dev = old.facts.map_or(0, |f| f.id.dev);
        let entries = entries.in_order();
        let mut items = Vec::with_capacity(entries.len());
        let mut o = 0;
        for (name, entry) in entries.by_name() {
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
                    entries: Entries::Each(entries.into_iter().collect()),
                }
            }
            State::NotCovered(why) => Seen::NotCovered(why),
        };
        self.reread(folder, record.facts, seen);
        Ok(())
    }

    /// Put a crawl of `folder`'s subtree in place of its block: slot 0 is
    /// the folder itself, every other slot a folder below it. Gives the
    /// number each slot was given.
    pub fn graft(&mut self, folder: FolderId, crawl: Vec<skwird_crawl::Crawled>) -> Vec<FolderId> {
        let Some(old) = self.tree().block(folder) else {
            return Vec::new();
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
        // `blocks_of` numbers the slots in order, the first before any
        // other.
        let mut given = Vec::with_capacity(crawl.len());
        let mut blocks = blocks_of(
            crawl,
            top,
            &mut |slot| {
                let id = if slot == 0 { folder } else { tree.alloc() };
                given.push(id);
                id
            },
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
        given
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
///
/// With [`Options::watcher`], each folder read within the watcher's
/// subtrees is watched before its entries are read: what came of each is
/// in [`Refreshed::watches`].
pub fn refresh(
    index: &mut Index,
    root: &mut Root,
    opts: &Options,
    cancel: &Cancel,
) -> Result<Refreshed, Error> {
    let start = Time::now();
    root.reread_mounts().map_err(Error::Root)?;
    let (ids, stored) = index.stored(|_| true);
    let checked = check(root, &stored, opts, cancel)?;
    let mut done = Refreshed {
        checked: stored.len(),
        ..Refreshed::default()
    };
    apply(
        index,
        root,
        opts,
        cancel,
        &ids,
        &stored,
        &[],
        checked.stale,
        &mut done,
    )?;
    let tells = root.fs_name() != "fuse";
    index.set_proof(if tells {
        Proof::Checked(start)
    } else {
        Proof::AsOf(start)
    });
    Ok(done)
}

/// Watch every folder of the index below `scope` and prove it current,
/// as [`refresh`] does, but through each folder's handle: each is opened,
/// watched, then compared with what was stored (`check_watched`). A
/// change before its watch moves its change time, and one after is
/// heard. Gives what came of each folder's watch, the stale ones read
/// again and the new ones crawled, watched too. The index's proof is not
/// changed: the subtree is live from when this began, if every folder in
/// it was watched.
pub fn install(
    index: &mut Index,
    root: &mut Root,
    opts: &Options,
    cancel: &Cancel,
    scope: &Path,
) -> Result<Refreshed, Error> {
    root.reread_mounts().map_err(Error::Root)?;
    let mut done = Refreshed::default();
    // A folder made since its parent was last read is not in the index:
    // the nearest folder above it held is read again first, so that it is
    // crawled, and watched, before it is installed.
    if index.folder_at(scope).is_none() {
        let mut above = scope.to_path_buf();
        let held = loop {
            if !above.pop() {
                break index.root();
            }
            if let Some(folder) = index.folder_at(&above) {
                break Some(folder);
            }
        };
        if let Some(folder) = held {
            done = reread_folders(index, root, opts, cancel, &[folder])?;
        }
    }
    let (ids, stored) = index.stored(|rel| rel.starts_with(scope));
    let (checked, watches) = check_watched(root, &stored, opts, cancel)?;
    done.checked += stored.len();
    done.watches.extend(
        ids.iter()
            .copied()
            .zip(watches)
            .filter(|(_, w)| *w != Watch::Unasked),
    );
    apply(
        index,
        root,
        opts,
        cancel,
        &ids,
        &stored,
        &[],
        checked.stale,
        &mut done,
    )?;
    Ok(done)
}

/// Read again each of `folders`, as a change heard in it asks, and crawl
/// each folder new in it; watched as [`refresh`] watches.
pub fn reread_folders(
    index: &mut Index,
    root: &Root,
    opts: &Options,
    cancel: &Cancel,
    folders: &[FolderId],
) -> Result<Refreshed, Error> {
    let whole: Vec<HeardFolder> = folders.iter().map(|&f| (f, None)).collect();
    reread_named(index, root, opts, cancel, &whole)
}

/// A folder heard, and the names its events gave: none to read it whole.
pub type HeardFolder = (FolderId, Option<HashSet<Box<[u8]>>>);

/// [`reread_folders`], a folder given the names its events gave stat-ing
/// those alone of what it held (and what is new in its listing): every
/// other entry is kept as it was, if its listing gives it the same inode
/// and kind (DESIGN 8). A folder given none is read whole.
pub fn reread_named(
    index: &mut Index,
    root: &Root,
    opts: &Options,
    cancel: &Cancel,
    folders: &[HeardFolder],
) -> Result<Refreshed, Error> {
    let mut ids = Vec::with_capacity(folders.len());
    let mut named = Vec::with_capacity(folders.len());
    let mut stored = Stored::with_capacity(folders.len(), folders.len() * 64);
    for (folder, names) in folders {
        if index.tree().loc(*folder).is_none() {
            continue;
        }
        let facts = index.facts_of(*folder);
        ids.push(*folder);
        named.push(names.as_ref());
        stored.push(
            &index.path_of(*folder),
            StoredFolder {
                id: facts.map(|f| f.id).unwrap_or_default(),
                ctime: facts.map(|f| f.ctime).unwrap_or_default(),
                again: true,
            },
        );
    }
    let stale = (0..ids.len()).map(|i| (i, Stale::Changed)).collect();
    let mut done = Refreshed::default();
    apply(
        index, root, opts, cancel, &ids, &stored, &named, stale, &mut done,
    )?;
    Ok(done)
}

/// Read again each folder of `stored` (numbered `ids`) the verdicts name,
/// and crawl
/// each folder new since; what changed in `done`.
#[allow(clippy::too_many_arguments)] // the parts of one refresh, shared by three
fn apply(
    index: &mut Index,
    root: &Root,
    opts: &Options,
    cancel: &Cancel,
    ids: &[FolderId],
    stored: &Stored,
    named: &[Option<&HashSet<Box<[u8]>>>],
    verdicts: Vec<(usize, Stale)>,
    done: &mut Refreshed,
) -> Result<(), Error> {
    let top = index.root();
    let mut fresh = Vec::new();
    for (i, stale) in verdicts {
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
        // Its block, decoded once for what it knew and the merge.
        let Some(old) = index.tree().block(folder) else {
            continue;
        };
        let (found, seen) = match stale {
            Stale::Mount => (None, Seen::NotCovered(Why::Mount)),
            Stale::NetworkMount => (None, Seen::NotCovered(Why::NetworkMount)),
            _ => {
                // Named by its events, it stats those entries alone, and
                // what is new; the others kept as they were read.
                let kept = Cell::new(0);
                let (found, outcome, watch) = match named.get(i).copied().flatten() {
                    Some(touched) => {
                        read_named(index, &old, root, &stored.rel(i), opts, touched, &kept)
                    }
                    None => read_folder_watched(root, &stored.rel(i), opts),
                };
                if watch != Watch::Unasked {
                    done.watches.push((folder, watch));
                }
                if let Outcome::Listed(listing) = &outcome {
                    done.entries += listing.len() - kept.get();
                    done.largest = done.largest.max(listing.len());
                }
                (found, Seen::of(outcome))
            }
        };
        let folder_facts = found.as_ref().map(facts);
        let state = match &seen {
            Seen::Listed { again, .. } => State::Listed { again: *again },
            Seen::NotCovered(why) => State::NotCovered(*why),
        };
        let size = match &seen {
            Seen::Listed { entries, .. } => entries.len(),
            Seen::NotCovered(_) => 0,
        };
        let reread = index.reread_block(&old, folder_facts, seen);
        match reread.written {
            Written::Patched => done.patched += 1,
            Written::Spliced => done.spliced += 1,
            Written::Cut => done.cut += 1,
            Written::Not => {}
        }
        fresh.extend(reread.fresh);
        done.records.push(Record::Folder(FolderRecord {
            path: stored.rel(i).to_path_buf(),
            facts: folder_facts.or_else(|| index.facts_of(folder)),
            state,
            changes: reread.changes,
        }));
        done.reread += 1;
        done.folders.push((folder, size));
    }
    while let Some(folder) = fresh.pop() {
        if index.tree().loc(folder).is_none() {
            continue;
        }
        let rel = index.path_of(folder);
        let crawl = crawl_below(root, &rel, opts, cancel)?;
        done.crawled += crawl.folders.len();
        done.records.extend(records_of(&crawl.folders));
        let watches: Vec<Watch> = crawl.folders.iter().map(|c| c.watch).collect();
        let given = index.graft(folder, crawl.folders);
        done.watches.extend(
            given
                .into_iter()
                .zip(watches)
                .filter(|(_, w)| *w != Watch::Unasked),
        );
    }
    Ok(())
}

/// Folder `old` read again at `rel`, its events having named `touched`:
/// those stat-ed, and what is new; every other entry taken as it was read
/// where the listing gives it the same inode and kind, counted in `kept`.
/// `reuse` is asked in the order of the names' bytes, as the crawl sorts
/// its listing, as the old entries are: both are walked once, no map made
/// of them. One asked out of order is found nowhere and stat-ed, never
/// taken wrongly.
fn read_named(
    index: &Index,
    old: &Block,
    root: &Root,
    rel: &Path,
    opts: &Options,
    touched: &HashSet<Box<[u8]>>,
    kept: &Cell<usize>,
) -> (Option<skwird_crawl::FolderStat>, Outcome, Watch) {
    let mut heard: Vec<&[u8]> = touched.iter().map(|n| &n[..]).collect();
    heard.sort_unstable();
    let table = index.names();
    let dev = old.facts.map_or(0, |f| f.id.dev);
    let (at, at_heard) = (Cell::new(0), Cell::new(0));
    read_folder_reusing(root, rel, opts, &|name, ino, kind| {
        let mut h = at_heard.get();
        while heard.get(h).is_some_and(|n| *n < name) {
            h += 1;
        }
        at_heard.set(h);
        if heard.get(h) == Some(&name) {
            return None;
        }
        let mut a = at.get();
        while old.items.get(a).is_some_and(|it| table.get(it.name) < name) {
            a += 1;
        }
        at.set(a);
        let item = old.items.get(a).filter(|it| table.get(it.name) == name)?;
        let entry = index.entry_of(item, dev);
        let same = entry.id.ino == ino && (kind == Kind::Other || entry.kind == kind);
        kept.set(kept.get() + usize::from(same));
        same.then_some(entry)
    })
}
