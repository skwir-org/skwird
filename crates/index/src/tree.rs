//! Folder blocks in depth-first order, cut into chunks, each chunk
//! rewritten alone (`docs/DESIGN.md`, decision 8).
//!
//! The blocks are in pre-order: a folder's block comes before its
//! descendants', and its whole subtree is one range, the folder's block
//! and every block after it that is deeper. That holds however chunks
//! are rewritten, because a change keeps it:
//!
//! - a folder made is inserted right after its parent's own block (pre-
//!   order allows any order of a folder's subtrees);
//! - a folder removed takes its range with it;
//! - a folder moved is its range taken out and inserted after its new
//!   parent's block, every depth in it moved alike.
//!
//! Each change rewrites the chunks it touches and no other: a chunk grown
//! past its limit is cut in two at a block, and one shrunk below its
//! floor takes in a neighbour. A folder keeps its number through all of
//! this; the folder table says where its block is now, and is copied a
//! page at a time ([`Pages`]), so a snapshot is cheap.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::block::{Block, FolderId};
use crate::chunk::Chunk;
use crate::pages::Pages;

/// A chunk's number, which names it as long as it lives.
pub type ChunkId = u32;

/// Where a folder's block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub chunk: ChunkId,
    pub block: u32,
}

/// How large chunks grow, in entries (a block counts its entries and one
/// for itself).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The size chunks are cut at.
    pub target: usize,
    /// Past this a chunk is cut, unless it holds a single block.
    pub max: usize,
    /// Below this a chunk takes in a neighbour.
    pub min: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            target: 16_384,
            max: 24_576,
            min: 4_096,
        }
    }
}

/// The weight of a block in a chunk.
fn weight(block: &Block) -> usize {
    block.items.len() + 1
}

/// The index's folders: chunks in order, and where each folder is.
#[derive(Clone, Debug)]
pub struct Tree {
    order: Vec<ChunkId>,
    chunks: HashMap<ChunkId, Arc<Chunk>>,
    locs: Pages<Option<Loc>>,
    /// Numbers let go of, given again first.
    free: Vec<FolderId>,
    next_chunk: ChunkId,
    limits: Limits,
    /// Its shape ([`Tree::shape`]).
    shape: u64,
}

/// The shapes given so far, in this process.
static SHAPES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_shape() -> u64 {
    SHAPES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Tree {
    /// A tree of `blocks`, given in pre-order, each with its folder's
    /// number already given.
    pub fn build(blocks: Vec<Block>, limits: Limits) -> Tree {
        let mut tree = Tree {
            order: Vec::new(),
            chunks: HashMap::new(),
            locs: Pages::new(None),
            free: Vec::new(),
            next_chunk: 0,
            limits,
            shape: next_shape(),
        };
        tree.place(0, 0, blocks);
        // Numbers below the highest given that no block holds are free.
        for id in 0..tree.locs.len() {
            if tree.locs.get(id).copied().flatten().is_none() {
                tree.free.push(id as FolderId);
            }
        }
        tree
    }

    /// The tree of chunks read back, in order, each with the number that
    /// names it: refused unless their blocks are one tree in pre-order,
    /// each folder once, each depth its parent's and one.
    pub fn from_chunks(
        chunks: Vec<(ChunkId, Arc<Chunk>)>,
        limits: Limits,
    ) -> Result<Tree, crate::packed::Malformed> {
        use crate::packed::Malformed;
        let mut tree = Tree {
            order: Vec::with_capacity(chunks.len()),
            chunks: HashMap::with_capacity(chunks.len()),
            locs: Pages::new(None),
            free: Vec::new(),
            next_chunk: chunks.iter().map(|(id, _)| id + 1).max().unwrap_or(0),
            limits,
            shape: next_shape(),
        };
        // The folders above the block being read, with their depths.
        let mut above: Vec<(FolderId, u32)> = Vec::new();
        let mut rooted = false;
        for (id, chunk) in chunks {
            if chunk.is_empty() || tree.chunks.contains_key(&id) {
                return Err(Malformed);
            }
            let view = chunk.view();
            for b in 0..chunk.len() {
                let (folder, depth, parent) = (view.folder(b), view.depth(b), view.parent(b));
                if tree.loc(folder).is_some() {
                    return Err(Malformed);
                }
                while above.last().is_some_and(|&(_, d)| d >= depth) {
                    above.pop();
                }
                match (above.last(), parent) {
                    (None, None) if depth == 0 && !rooted => rooted = true,
                    (Some(&(top, d)), Some(p)) if top == p && depth == d + 1 => {}
                    _ => return Err(Malformed),
                }
                above.push((folder, depth));
                tree.locs.set(
                    folder as usize,
                    Some(Loc {
                        chunk: id,
                        block: b as u32,
                    }),
                );
            }
            tree.order.push(id);
            tree.chunks.insert(id, chunk);
        }
        for f in 0..tree.locs.len() {
            if tree.locs.get(f).copied().flatten().is_none() {
                tree.free.push(f as FolderId);
            }
        }
        Ok(tree)
    }

    /// The chunks' numbers, in order.
    pub fn chunk_ids(&self) -> &[ChunkId] {
        &self.order
    }

    /// Put `chunk`, the same blocks read another way (its file, mapped),
    /// in place of chunk `id`.
    pub fn set_chunk(&mut self, id: ChunkId, chunk: Arc<Chunk>) {
        debug_assert_eq!(self.chunks[&id].len(), chunk.len());
        self.chunks.insert(id, chunk);
    }

    /// Which folders it holds, where: a number that changes whenever a
    /// folder is made, moved or let go of, and that no other tree of this
    /// process has had. While it stays, each folder is where it was, with
    /// the number it had.
    pub fn shape(&self) -> u64 {
        self.shape
    }

    /// A number for a folder to be made.
    pub fn alloc(&mut self) -> FolderId {
        if let Some(id) = self.free.pop() {
            return id;
        }
        let id = self.locs.len() as FolderId;
        self.locs.set(id as usize, None);
        id
    }

    pub fn loc(&self, folder: FolderId) -> Option<Loc> {
        self.locs.get(folder as usize).copied().flatten()
    }

    /// The chunks, in order.
    pub fn chunks(&self) -> impl Iterator<Item = (ChunkId, &Arc<Chunk>)> {
        self.order.iter().map(|id| (*id, &self.chunks[id]))
    }

    pub fn chunk(&self, id: ChunkId) -> &Arc<Chunk> {
        &self.chunks[&id]
    }

    /// The folders' blocks, in all.
    pub fn len(&self) -> usize {
        self.chunks.values().map(|c| c.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The block of `folder`.
    pub fn block(&self, folder: FolderId) -> Option<Block> {
        let loc = self.loc(folder)?;
        Some(self.chunks[&loc.chunk].block(loc.block as usize))
    }

    fn position(&self, chunk: ChunkId) -> usize {
        self.order
            .iter()
            .position(|&c| c == chunk)
            .expect("a chunk a folder is in is in the order")
    }

    /// The blocks of `folder`'s subtree, as ranges of blocks in chunks, in
    /// order: its own block, and every block after it that is deeper.
    pub fn ranges(&self, folder: FolderId) -> Vec<(ChunkId, Range<usize>)> {
        let Some(start) = self.loc(folder) else {
            return Vec::new();
        };
        let first = &self.chunks[&start.chunk];
        let depth = first.depth(start.block as usize);
        // The root's subtree is everything: no depth need be read.
        if depth == 0 {
            return self
                .order
                .iter()
                .map(|&id| (id, 0..self.chunks[&id].len()))
                .collect();
        }
        let mut ranges = Vec::new();
        let mut from = start.block as usize + 1;
        for &id in &self.order[self.position(start.chunk)..] {
            let chunk = &self.chunks[&id];
            let view = chunk.view();
            let mut end = from;
            while end < chunk.len() && view.depth(end) > depth {
                end += 1;
            }
            let begin = if id == start.chunk {
                start.block as usize
            } else {
                from
            };
            if end > begin {
                ranges.push((id, begin..end));
            }
            if end < chunk.len() {
                break;
            }
            from = 0;
        }
        ranges
    }

    /// Put `block` in place of its folder's.
    pub fn replace(&mut self, block: Block) {
        let loc = self
            .loc(block.folder)
            .expect("a block replaced is in the tree");
        let at = self.position(loc.chunk);
        let mut blocks = self.chunks[&loc.chunk].decode();
        blocks[loc.block as usize] = block;
        self.place(at, 1, blocks);
    }

    /// Insert `blocks`, folders' subtrees in pre-order, right after the
    /// own block of `parent`: their depths and the top ones' parents must
    /// already say so, and their numbers come from [`Tree::alloc`].
    pub fn insert_after(&mut self, parent: FolderId, blocks: Vec<Block>) {
        self.shape = next_shape();
        let loc = self.loc(parent).expect("a parent is in the tree");
        let at = self.position(loc.chunk);
        let mut all = self.chunks[&loc.chunk].decode();
        let tail = all.split_off(loc.block as usize + 1);
        all.extend(blocks);
        all.extend(tail);
        self.place(at, 1, all);
    }

    /// Take `folder`'s subtree out, in pre-order: its numbers stay given,
    /// for it to be inserted elsewhere ([`Tree::insert_after`]).
    pub fn take(&mut self, folder: FolderId) -> Vec<Block> {
        self.shape = next_shape();
        let ranges = self.ranges(folder);
        let Some((first, _)) = ranges.first() else {
            return Vec::new();
        };
        let at = self.position(*first);
        let mut kept = Vec::new();
        let mut taken = Vec::new();
        for (i, (id, range)) in ranges.iter().enumerate() {
            let mut blocks = self.chunks[id].decode();
            let after = blocks.split_off(range.end);
            let inside = blocks.split_off(range.start);
            if i == 0 {
                kept.extend(blocks);
            }
            taken.extend(inside);
            if i == ranges.len() - 1 {
                kept.extend(after);
            }
        }
        for block in &taken {
            self.locs.set(block.folder as usize, None);
        }
        self.place(at, ranges.len(), kept);
        taken
    }

    /// Remove `folder`'s subtree, and let go of its numbers.
    pub fn remove(&mut self, folder: FolderId) {
        for block in self.take(folder) {
            self.free.push(block.folder);
        }
    }

    /// Put `blocks` in place of the `remove` chunks at position `at` of
    /// the order, cut into chunks by the limits. A chunk ends up below the
    /// floor only where no neighbour can take it in within the limit: the
    /// cut's own small groups join their neighbours, and so do the chunks
    /// on either side of what is placed, taken in and cut again.
    fn place(&mut self, mut at: usize, mut remove: usize, mut blocks: Vec<Block>) {
        let groups = loop {
            let groups = self.cut(std::mem::take(&mut blocks));
            let first = groups.first().map_or(0, |g| g.iter().map(weight).sum());
            let last = groups.last().map_or(0, |g| g.iter().map(weight).sum());
            let previous = (at > 0).then(|| self.chunks[&self.order[at - 1]].items());
            let next = (at + remove < self.order.len())
                .then(|| self.chunks[&self.order[at + remove]].items());
            let joins = |ours: usize, theirs: usize| {
                (ours < self.limits.min || theirs < self.limits.min)
                    && ours + theirs <= self.limits.max
            };
            let join_previous = previous.is_some_and(|p| joins(first, p));
            let join_next = next.is_some_and(|n| joins(last, n));
            if !join_previous && !join_next {
                break groups;
            }
            blocks = groups.into_iter().flatten().collect();
            if join_previous {
                let mut taken = self.chunks[&self.order[at - 1]].decode();
                taken.append(&mut blocks);
                blocks = taken;
                at -= 1;
                remove += 1;
            } else {
                blocks.extend(self.chunks[&self.order[at + remove]].decode());
                remove += 1;
            }
        };
        let made: Vec<ChunkId> = groups
            .into_iter()
            .map(|group| {
                let id = self.next_chunk;
                self.next_chunk += 1;
                for (i, block) in group.iter().enumerate() {
                    self.locs.set(
                        block.folder as usize,
                        Some(Loc {
                            chunk: id,
                            block: i as u32,
                        }),
                    );
                }
                self.chunks.insert(id, Arc::new(Chunk::new(&group)));
                id
            })
            .collect();
        for old in self.order.splice(at..at + remove, made) {
            self.chunks.remove(&old);
        }
    }

    /// `blocks` cut at blocks into groups near the target, none past the
    /// limit but a single block, and none below the floor that a
    /// neighbouring group could take in within the limit.
    fn cut(&self, blocks: Vec<Block>) -> Vec<Vec<Block>> {
        let total: usize = blocks.iter().map(weight).sum();
        if blocks.is_empty() {
            return Vec::new();
        }
        if total <= self.limits.max {
            return vec![blocks];
        }
        let each = total.div_ceil(total.div_ceil(self.limits.target.max(1)));
        let mut groups: Vec<(usize, Vec<Block>)> = Vec::new();
        for block in blocks {
            let w = weight(&block);
            match groups.last_mut() {
                Some((filled, group)) if *filled + w <= each => {
                    *filled += w;
                    group.push(block);
                }
                _ => groups.push((w, vec![block])),
            }
        }
        let mut i = 0;
        while i < groups.len() {
            let w = groups[i].0;
            let fits = |j: usize| w + groups[j].0 <= self.limits.max;
            let small = |j: usize| w < self.limits.min || groups[j].0 < self.limits.min;
            if i > 0 && small(i - 1) && fits(i - 1) {
                let (w, mut group) = groups.remove(i);
                groups[i - 1].0 += w;
                groups[i - 1].1.append(&mut group);
                i -= 1;
            } else if i + 1 < groups.len() && small(i + 1) && fits(i + 1) {
                let (w, group) = groups.remove(i);
                groups[i].0 += w;
                let mut after = std::mem::replace(&mut groups[i].1, group);
                groups[i].1.append(&mut after);
            } else {
                i += 1;
            }
        }
        groups.into_iter().map(|(_, g)| g).collect()
    }

    /// Check what the tree promises, for tests: every folder's block is
    /// where the table says, chunks are within their limits, and depths
    /// follow parents.
    #[cfg(test)]
    pub(crate) fn assert_sound(&self) {
        let mut depth_of: HashMap<FolderId, u32> = HashMap::new();
        for (index, &id) in self.order.iter().enumerate() {
            let chunk = &self.chunks[&id];
            assert!(!chunk.is_empty(), "an empty chunk");
            let items = chunk.items();
            assert!(
                items <= self.limits.max || chunk.len() == 1,
                "chunk {index} holds {items} past the limit"
            );
            let neighbours = [index.checked_sub(1), Some(index + 1)];
            for n in neighbours.into_iter().flatten() {
                if let Some(other) = self.order.get(n) {
                    let theirs = self.chunks[other].items();
                    assert!(
                        (items >= self.limits.min && theirs >= self.limits.min)
                            || items + theirs > self.limits.max,
                        "chunks {index} and {n} ({items} and {theirs}) would join"
                    );
                }
            }
            for b in 0..chunk.len() {
                let block = chunk.block(b);
                assert_eq!(
                    self.loc(block.folder),
                    Some(Loc {
                        chunk: id,
                        block: b as u32
                    })
                );
                let expect = block.parent.map_or(0, |p| depth_of[&p] + 1);
                assert_eq!(block.depth, expect, "depth of {}", block.folder);
                depth_of.insert(block.folder, block.depth);
            }
        }
    }
}

#[cfg(test)]
mod tests;
