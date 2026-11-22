//! An index: its folders' tree and its names, and the queries on them
//! (`docs/DESIGN.md`, decision 7). Queries are data: names matched as
//! skwir matches them, kinds, ranges of size and of modification time,
//! all under one folder, whose subtree is one range of the index.

use std::collections::HashMap;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use skwird_crawl::{Attrs, FileId, Kind, Time};

use crate::block::{FolderId, NameId};
use crate::chunk::{View, ZONE, kind_code};
use crate::matching::{Matcher, NameQuery};
use crate::names::Names;
use crate::packed::RUN;
use crate::packed::from_signed;
use crate::tree::ChunkId;
use crate::tree::Tree;
use crate::update::Proof;

/// What an asker may see of a folder's entries (DESIGN 4): seen, hidden
/// from them, or not known (an ACL not evaluated), never answered then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sight {
    Seen,
    Hidden,
    Unknown,
}

impl Sight {
    /// Both: hidden if either is, unknown if either is not known.
    #[must_use]
    pub fn and(self, other: Sight) -> Sight {
        match (self, other) {
            (Sight::Hidden, _) | (_, Sight::Hidden) => Sight::Hidden,
            (Sight::Unknown, _) | (_, Sight::Unknown) => Sight::Unknown,
            _ => Sight::Seen,
        }
    }
}

/// What an asker may see of each folder's entries, given a memo of its
/// own for the thread that asks.
pub type SightOf<'a> = &'a (dyn Fn(FolderId, &mut HashMap<FolderId, Sight>) -> Sight + Sync);

/// What to look for below a folder. Every part given must hold.
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub name: Option<NameQuery>,
    pub kinds: Option<Vec<Kind>>,
    /// Sizes, in bytes. An entry whose attributes the index does not hold
    /// (a mount point) matches no size.
    pub size: Option<RangeInclusive<u64>>,
    /// Modification times. As for sizes, an entry without attributes
    /// matches none.
    pub mtime: Option<RangeInclusive<Time>>,
    /// At most this many answers, in the index's order.
    pub limit: Option<usize>,
}

/// An entry that answers a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Its path below the index's root.
    pub path: PathBuf,
    pub kind: Kind,
    pub id: FileId,
    /// Its folder's identity.
    pub folder: FileId,
    pub attrs: Option<Attrs>,
}

/// The index of one root.
#[derive(Clone, Debug)]
pub struct Index {
    tree: Tree,
    names: Names,
    proof: Proof,
    /// When it was last crawled whole: its attributes are as of then.
    crawled_at: Time,
    /// The subtrees live, by path below the root, each since when
    /// ([`crate::keeper::Live`]): shared by snapshots.
    live: Arc<[(PathBuf, Time)]>,
    /// The access ACLs of its folders that have one, by identity, as the
    /// system service reads them ([`skwird_crawl::ACL`]): shared by
    /// snapshots, so that answering reads none.
    acls: Arc<HashMap<FileId, skwird_crawl::acl::Acl>>,
}

/// A name's bytes as a path's component.
#[cfg(unix)]
pub fn component(name: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(name))
}

/// A name's bytes as a path's component: elsewhere than Unix, its UTF-8.
#[cfg(not(unix))]
pub fn component(name: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(name).into_owned())
}

impl Index {
    /// An index of `tree` and `names`, proven as of no time at all until
    /// told ([`Index::set_proof`]).
    pub fn new(tree: Tree, names: Names) -> Index {
        Index {
            tree,
            names,
            proof: Proof::AsOf(Time::default()),
            crawled_at: Time::default(),
            live: Arc::from([]),
            acls: Arc::default(),
        }
    }

    pub fn proof(&self) -> Proof {
        self.proof
    }

    pub fn set_proof(&mut self, proof: Proof) {
        self.proof = proof;
    }

    pub fn crawled_at(&self) -> Time {
        self.crawled_at
    }

    pub fn set_crawled_at(&mut self, at: Time) {
        self.crawled_at = at;
    }

    /// The access ACLs of its folders, by identity.
    pub fn set_acls(&mut self, acls: Arc<HashMap<FileId, skwird_crawl::acl::Acl>>) {
        self.acls = acls;
    }

    /// The access ACL of the folder of identity `id`, if it has one and it
    /// was read.
    pub fn acl_of(&self, id: FileId) -> Option<&skwird_crawl::acl::Acl> {
        self.acls.get(&id)
    }

    /// The subtrees live, each since when.
    pub fn set_live(&mut self, live: Vec<(PathBuf, Time)>) {
        self.live = Arc::from(live);
    }

    /// Since when every change at or below `rel` is heard, if it is within
    /// a live subtree: the earliest of those holding it.
    pub fn live_since(&self, rel: &Path) -> Option<Time> {
        self.live
            .iter()
            .filter(|(scope, _)| rel.starts_with(scope))
            .map(|&(_, since)| since)
            .min()
    }

    /// The tree and the names, each to be changed.
    pub fn parts_mut(&mut self) -> (&mut Tree, &mut Names) {
        (&mut self.tree, &mut self.names)
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    pub fn names(&self) -> &Names {
        &self.names
    }

    pub fn names_mut(&mut self) -> &mut Names {
        &mut self.names
    }

    /// The root's folder: the first block's.
    pub fn root(&self) -> Option<FolderId> {
        self.tree.chunks().next().map(|(_, c)| c.view().folder(0))
    }

    /// The folder at `rel` below the root, by its names: none if it is
    /// not in the index, or is not a folder.
    pub fn folder_at(&self, rel: &Path) -> Option<FolderId> {
        let mut folder = self.root()?;
        for part in rel.components() {
            let name = part.as_os_str().as_encoded_bytes();
            let loc = self.tree.loc(folder)?;
            let view = self.tree.chunk(loc.chunk).view();
            let entries = view.entries(loc.block as usize);
            // Entries are in the order of their names' bytes.
            let (mut lo, mut hi) = (entries.start, entries.end);
            while lo < hi {
                let mid = usize::midpoint(lo, hi);
                if self.names.get(view.item_name(mid)) < name {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo == entries.end || self.names.get(view.item_name(lo)) != name {
                return None;
            }
            folder = view.item_child(lo)?;
        }
        Some(folder)
    }

    /// A folder's path below the root.
    pub fn path_of(&self, folder: FolderId) -> PathBuf {
        let mut names: Vec<NameId> = Vec::new();
        let mut at = Some(folder);
        while let Some(f) = at {
            let Some(loc) = self.tree.loc(f) else { break };
            let view = self.tree.chunk(loc.chunk).view();
            let parent = view.parent(loc.block as usize);
            if parent.is_some() {
                names.push(view.name(loc.block as usize));
            }
            at = parent;
        }
        names
            .iter()
            .rev()
            .map(|&n| component(self.names.get(n)))
            .collect()
    }

    /// Which names match, a bit each, scored on threads of their own.
    fn matching(&self, matcher: &Matcher) -> Vec<u64> {
        let count = self.names.len();
        let words = count.div_ceil(64);
        let threads = std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .clamp(1, 8);
        let per = words.div_ceil(threads).max(1);
        let names = &self.names;
        std::thread::scope(|scope| {
            let parts: Vec<_> = (0..words)
                .step_by(per)
                .map(|from| {
                    scope.spawn(move || {
                        let to = (from + per).min(words);
                        let mut bits = vec![0u64; to - from];
                        for w in from..to {
                            for b in 0..64 {
                                let id = w * 64 + b;
                                if id >= count {
                                    break;
                                }
                                let id = id as NameId;
                                if matcher.could(names.mask(id)) && matcher.matches(names.get(id)) {
                                    bits[w - from] |= 1 << b;
                                }
                            }
                        }
                        bits
                    })
                })
                .collect();
            parts
                .into_iter()
                .flat_map(|p| p.join().expect("a scoring thread does not panic"))
                .collect()
        })
    }

    /// The entries below `root` that answer `query`, in the index's
    /// order, or why the query cannot run (a glob that does not compile).
    /// The names are scored once; the subtree's ranges are then scanned
    /// on threads of their own, a run of 64 entries at a time, passing
    /// over every zone whose sizes, times or kinds cannot answer.
    pub fn query(&self, root: FolderId, query: &Query) -> Result<Vec<Hit>, String> {
        self.query_seen(root, query, None).map(|(hits, _)| hits)
    }

    /// [`Index::query`] for an asker (DESIGN 4): an entry answers only if
    /// `sight` says its folder's entries are seen. An entry that would
    /// answer, in a folder whose sight is not known, is withheld, and its
    /// folder given back: not covered for this answer.
    pub fn query_seen(
        &self,
        root: FolderId,
        query: &Query,
        sight: Option<SightOf<'_>>,
    ) -> Result<(Vec<Hit>, Vec<FolderId>), String> {
        let names = query
            .name
            .as_ref()
            .map(|q| Matcher::new(q).map(|m| self.matching(&m)))
            .transpose()?;
        let filter = Filter {
            names: names.as_deref(),
            kinds: query.kinds.as_ref().map_or(0b1111, |ks| {
                ks.iter().fold(0, |m, &k| m | 1 << kind_code(k))
            }),
            times: query
                .mtime
                .as_ref()
                .map(|r| from_signed(r.start().sec)..=from_signed(r.end().sec)),
            query,
            sight,
        };
        let limit = query.limit.unwrap_or(usize::MAX);
        let ranges = self.tree.ranges(root);
        let threads = std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .clamp(1, 8)
            .min(ranges.len().max(1));
        let per = ranges.len().div_ceil(threads).max(1);
        let parts: Vec<Found> = std::thread::scope(|scope| {
            let filter = &filter;
            let handles: Vec<_> = ranges
                .chunks(per)
                .map(|group| {
                    scope.spawn(move || {
                        let mut found = Found::default();
                        for (chunk, blocks) in group {
                            if found.hits.len() >= limit {
                                break;
                            }
                            self.scan(*chunk, blocks.clone(), filter, limit, &mut found);
                        }
                        found
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a scanning thread does not panic"))
                .collect()
        });
        let mut hits = Vec::new();
        let mut unknown = std::collections::BTreeSet::new();
        for part in parts {
            hits.extend(part.hits);
            unknown.extend(part.unknown);
        }
        hits.truncate(limit);
        Ok((hits, unknown.into_iter().collect()))
    }

    /// Scan the entries of `blocks` in `chunk`, adding those that answer.
    fn scan(
        &self,
        chunk: ChunkId,
        blocks: Range<usize>,
        filter: &Filter<'_>,
        limit: usize,
        found: &mut Found,
    ) {
        let view = self.tree.chunk(chunk).view();
        let items = view.entries(blocks.start).start..view.entries(blocks.end - 1).end;
        let names_col = view.item_names();
        let kinds_col = view.item_kinds();
        let mut names = [0u64; RUN];
        let mut kinds = [0u64; RUN];
        let mut i = items.start;
        while i < items.end {
            let zone = i / ZONE;
            let zone_end = ((zone + 1) * ZONE).min(items.end);
            let (largest, tmin, tmax, present) = view.zone(zone);
            let passed = present & filter.kinds == 0
                || filter
                    .query
                    .size
                    .as_ref()
                    .is_some_and(|r| largest < *r.start())
                || filter
                    .times
                    .as_ref()
                    .is_some_and(|r| tmax < *r.start() || tmin > *r.end());
            if passed {
                i = zone_end;
                continue;
            }
            // A zone's first entry begins a run: zones are runs' multiples.
            while i < zone_end {
                let run = i / RUN;
                let first = run * RUN;
                let end = (first + RUN).min(zone_end);
                if filter.names.is_some() {
                    names_col.decode_run(run, &mut names);
                }
                if filter.kinds != 0b1111 {
                    kinds_col.decode_run(run, &mut kinds);
                }
                for j in i..end {
                    if let Some(bits) = filter.names {
                        let n = names[j - first] as usize;
                        if bits[n / 64] & (1 << (n % 64)) == 0 {
                            continue;
                        }
                    }
                    if filter.kinds != 0b1111 && (1 << (kinds[j - first] & 3)) & filter.kinds == 0 {
                        continue;
                    }
                    if filter.attrs_answer(&view, j) {
                        if found.hits.len() >= limit {
                            return;
                        }
                        match found.sight(filter, view.folder(view.block_of(j))) {
                            Sight::Seen => {
                                let hit = self.hit(&view, j, &mut found.paths);
                                found.hits.push(hit);
                            }
                            Sight::Hidden => {}
                            Sight::Unknown => {
                                found.unknown.insert(view.folder(view.block_of(j)));
                            }
                        }
                    }
                }
                i = end;
            }
        }
    }

    fn hit(&self, view: &View<'_>, j: usize, paths: &mut HashMap<FolderId, PathBuf>) -> Hit {
        let b = view.block_of(j);
        let folder = view.folder(b);
        let path = paths
            .entry(folder)
            .or_insert_with(|| self.path_of(folder))
            .join(component(self.names.get(view.item_name(j))));
        let item = view.item(j);
        let folder_id = view.facts(b).map(|f| f.id).unwrap_or_default();
        // A folder's device is its own block's: a btrfs subvolume's top
        // has a device of its own.
        let dev = item
            .child
            .and_then(|c| self.facts_of(c))
            .map_or(folder_id.dev, |f| f.id.dev);
        Hit {
            path,
            kind: item.kind,
            id: FileId { dev, ino: item.ino },
            folder: folder_id,
            attrs: item.attrs,
        }
    }

    /// What the index knows of a folder's entries, read from its block
    /// alone.
    pub fn state_of(&self, folder: FolderId) -> Option<crate::block::State> {
        let loc = self.tree.loc(folder)?;
        Some(self.tree.chunk(loc.chunk).view().state(loc.block as usize))
    }

    /// The folder holding `folder`: none for the root.
    pub fn parent_of(&self, folder: FolderId) -> Option<FolderId> {
        let loc = self.tree.loc(folder)?;
        self.tree.chunk(loc.chunk).view().parent(loc.block as usize)
    }

    /// The folders below `root`, itself included, whose mode says they
    /// have an access ACL ([`skwird_crawl::ACL`]): read from their blocks,
    /// no entry read.
    pub fn acl_folders_below(&self, root: FolderId) -> Vec<FolderId> {
        let mut out = Vec::new();
        for (chunk, blocks) in self.tree.ranges(root) {
            let view = self.tree.chunk(chunk).view();
            for b in blocks {
                if view
                    .facts(b)
                    .is_some_and(|f| f.mode & skwird_crawl::ACL != 0)
                {
                    out.push(view.folder(b));
                }
            }
        }
        out
    }

    /// The folders `folder` holds, from its block alone.
    pub fn subfolders(&self, folder: FolderId) -> Vec<FolderId> {
        let Some(loc) = self.tree.loc(folder) else {
            return Vec::new();
        };
        let view = self.tree.chunk(loc.chunk).view();
        view.entries(loc.block as usize)
            .filter_map(|j| view.item(j).child)
            .collect()
    }

    /// A folder's facts, read from its block alone.
    pub fn facts_of(&self, folder: FolderId) -> Option<crate::block::Facts> {
        let loc = self.tree.loc(folder)?;
        self.tree.chunk(loc.chunk).view().facts(loc.block as usize)
    }

    /// Let go of every name that no entry and no folder holds any more:
    /// each is kept empty, its number kept, so that no chunk changes. A
    /// purge does this, so that a private folder's names are not kept in
    /// the names' files; the next save writes them all again.
    pub fn blank_dead_names(&mut self) {
        let count = self.names.len();
        let mut live = vec![0u64; count.div_ceil(64)];
        let mut hold = |id: NameId| live[id as usize / 64] |= 1 << (id % 64);
        hold(0);
        for (_, chunk) in self.tree.chunks() {
            let view = chunk.view();
            for b in 0..chunk.len() {
                hold(view.name(b));
                for i in view.entries(b) {
                    hold(view.item_name(i));
                }
            }
        }
        let names: Vec<&[u8]> = (0..count as NameId)
            .map(|id| {
                if live[id as usize / 64] & (1 << (id % 64)) == 0 {
                    &[][..]
                } else {
                    self.names.get(id)
                }
            })
            .collect();
        let bytes = crate::names::segment_bytes(&names);
        let segment = crate::names::Segment::from_bytes(
            crate::chunk::Bytes::Owned(bytes.into_boxed_slice()),
            true,
            None,
        )
        .expect("names just written read back");
        self.names = Names::new(segment);
    }

    /// What the index holds, and its bytes.
    pub fn sizes(&self) -> Sizes {
        let mut s = Sizes {
            names: self.names.len() as u64,
            names_bytes: self.names.bytes_len(),
            ..Sizes::default()
        };
        for (_, chunk) in self.tree.chunks() {
            s.chunks += 1;
            s.folders += chunk.len() as u64;
            s.entries += chunk.entries() as u64;
            s.chunk_bytes += chunk.bytes().len() as u64;
        }
        s
    }
}

/// An index's counts and bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub chunks: u64,
    pub folders: u64,
    pub entries: u64,
    pub names: u64,
    pub chunk_bytes: u64,
    pub names_bytes: u64,
}

/// A query's parts, compiled for a scan.
struct Filter<'a> {
    /// Which names answer, a bit each.
    names: Option<&'a [u64]>,
    /// Which kinds answer, a bit each.
    kinds: u64,
    /// The modification times' seconds that answer, as zones keep them.
    times: Option<RangeInclusive<u64>>,
    query: &'a Query,
    /// What the asker may see, if answers are for an asker.
    sight: Option<SightOf<'a>>,
}

/// What one scanning thread found: its hits, its folders' paths, what the
/// asker may see of each folder, and the folders whose sight is not known
/// that held an entry that would answer.
#[derive(Default)]
struct Found {
    hits: Vec<Hit>,
    paths: HashMap<FolderId, PathBuf>,
    seen: HashMap<FolderId, Sight>,
    /// The last folder asked of, and its sight: entries come a folder at
    /// a time.
    last: Option<(FolderId, Sight)>,
    unknown: std::collections::BTreeSet<FolderId>,
}

impl Found {
    fn sight(&mut self, filter: &Filter<'_>, folder: FolderId) -> Sight {
        let Some(sight) = filter.sight else {
            return Sight::Seen;
        };
        if let Some((f, s)) = self.last
            && f == folder
        {
            return s;
        }
        let s = sight(folder, &mut self.seen);
        self.last = Some((folder, s));
        s
    }
}

impl Filter<'_> {
    /// Whether entry `j`'s size and time answer: any entry where neither is
    /// asked; none whose attributes the index does not hold where one is.
    fn attrs_answer(&self, view: &View<'_>, j: usize) -> bool {
        let q = self.query;
        if q.size.is_none() && q.mtime.is_none() {
            return true;
        }
        let Some(attrs) = view.item(j).attrs else {
            return false;
        };
        q.size.as_ref().is_none_or(|r| r.contains(&attrs.size))
            && q.mtime.as_ref().is_none_or(|r| r.contains(&attrs.mtime))
    }
}
