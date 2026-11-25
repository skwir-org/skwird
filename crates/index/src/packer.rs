//! A crawl's folders packed as they come, and the index made of them once
//! the crawl ends (`docs/PLAN.md`, phase 1, step 7).
//!
//! A first build is the engine's largest moment, and its peak is the
//! crawl held whole: a listing of every folder, each entry with its name
//! in an allocation of its own. Here each folder is packed as it is read
//! into a few buffers that only grow, its listing let go of at once:
//! - the names, each once, end to end, with the hash of each;
//! - each folder's block, its numbers as variable-length integers;
//! - by slot, where each block starts, its parent and its name.
//!
//! When the crawl ends, the blocks are unpacked in pre-order, a chunk at a
//! time, each folder's children linked by name, and cut into chunks as
//! [`Tree::build`] cuts them.
//!
//! Large buffers, not many small allocations, so that what is freed goes
//! back to the system. Freed small allocations stay with the allocator: a
//! first build of 6.9 million entries left an engine of 1.9 GB, against
//! 177 MB for the same index read back (DESIGN 10).

use std::collections::HashMap;
use std::collections::hash_map::Entry as Slot;
use std::hash::{DefaultHasher, Hasher};
use std::sync::Arc;

use skwird_crawl::{Attrs, Crawled, FileId, Kind, Outcome, Time, Watch};

use crate::block::{Block, Facts, FolderId, Item, NameId, State};
use crate::build::facts;
use crate::chunk::{Bytes, Chunk, kind_code, kind_of, state_code, state_of};
use crate::index::Index;
use crate::names::{Names, Segment, segment_of};
use crate::tree::{Limits, Tree};

/// No slot: the root's parent, or a slot not handed yet.
const NONE: u32 = u32::MAX;

/// Names, each once, numbered as they are given: the empty name, the
/// root's, first.
struct Interner {
    /// Each name's number, by the hash of its bytes.
    by_hash: HashMap<u64, NameId>,
    /// The names whose hash another name has: rare, and correct.
    spill: HashMap<Box<[u8]>, NameId>,
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

fn hash_of(name: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(name);
    h.finish()
}

impl Interner {
    fn new() -> Interner {
        let mut names = Interner {
            by_hash: HashMap::new(),
            spill: HashMap::new(),
            bytes: Vec::new(),
            ends: Vec::new(),
        };
        names.intern(b"");
        names
    }

    fn get<'a>(bytes: &'a [u8], ends: &[usize], id: NameId) -> &'a [u8] {
        let i = id as usize;
        let start = if i == 0 { 0 } else { ends[i - 1] };
        &bytes[start..ends[i]]
    }

    fn intern(&mut self, name: &[u8]) -> NameId {
        let next = self.ends.len() as NameId;
        match self.by_hash.entry(hash_of(name)) {
            Slot::Occupied(seen) => {
                let id = *seen.get();
                if Interner::get(&self.bytes, &self.ends, id) == name {
                    return id;
                }
                if let Some(&id) = self.spill.get(name) {
                    return id;
                }
                self.spill.insert(name.into(), next);
            }
            Slot::Vacant(free) => {
                free.insert(next);
            }
        }
        self.bytes.extend_from_slice(name);
        self.ends.push(self.bytes.len());
        next
    }

    /// The names' file.
    fn finish(self) -> Vec<u8> {
        let Interner { bytes, ends, .. } = self;
        segment_of(ends.len(), |i| Interner::get(&bytes, &ends, i as NameId))
    }
}

/// Where a folder's block is, and what places it.
#[derive(Clone, Copy)]
struct Place {
    /// Where its block starts in the packed blocks: `usize::MAX` for a
    /// slot not handed.
    at: usize,
    parent: u32,
    name: NameId,
}

const UNSEEN: Place = Place {
    at: usize::MAX,
    parent: NONE,
    name: 0,
};

/// A crawl's folders, packed as they come (see the module).
pub struct Packer {
    names: Interner,
    packed: Vec<u8>,
    places: Vec<Place>,
    /// Every block's weight: its entries, and one for itself.
    weight: usize,
    /// The order of a listing's entries by their names' bytes, kept from
    /// one folder to the next.
    order: Vec<u32>,
    /// What came of watching each folder, where a watch was asked: a
    /// folder's number is its slot.
    watches: Vec<(FolderId, Watch)>,
}

impl Default for Packer {
    fn default() -> Self {
        Packer {
            names: Interner::new(),
            packed: Vec::new(),
            places: Vec::new(),
            weight: 0,
            order: Vec::new(),
            watches: Vec::new(),
        }
    }
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn take(bytes: &[u8], at: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = bytes[*at];
        *at += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

#[allow(clippy::cast_possible_wrap)] // a zigzag's bits, read back as they were
fn zig(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[allow(clippy::cast_possible_wrap)] // a zigzag's bits, read back as they were
fn unzig(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn put_time(out: &mut Vec<u8>, t: Time) {
    put(out, zig(t.sec));
    put(out, u64::from(t.nsec));
}

fn take_time(bytes: &[u8], at: &mut usize) -> Time {
    Time {
        sec: unzig(take(bytes, at)),
        nsec: take(bytes, at) as u32,
    }
}

fn put_facts(out: &mut Vec<u8>, facts: Option<Facts>) {
    let Some(f) = facts else {
        out.push(0);
        return;
    };
    out.push(1);
    put(out, f.id.dev);
    put(out, f.id.ino);
    put_time(out, f.ctime);
    put_time(out, f.mtime);
    put(out, u64::from(f.owner));
    put(out, u64::from(f.group));
    put(out, u64::from(f.mode));
}

fn take_facts(bytes: &[u8], at: &mut usize) -> Option<Facts> {
    let known = bytes[*at] == 1;
    *at += 1;
    known.then(|| Facts {
        id: FileId {
            dev: take(bytes, at),
            ino: take(bytes, at),
        },
        ctime: take_time(bytes, at),
        mtime: take_time(bytes, at),
        owner: take(bytes, at) as u32,
        group: take(bytes, at) as u32,
        mode: take(bytes, at) as u16,
    })
}

fn put_state(out: &mut Vec<u8>, state: State) {
    let (code, errno) = state_code(state);
    put(out, code);
    put(out, errno);
}

fn take_state(bytes: &[u8], at: &mut usize) -> State {
    let code = take(bytes, at);
    let errno = take(bytes, at);
    state_of(code, errno).expect("a state packed reads back")
}

fn put_item(out: &mut Vec<u8>, name: NameId, kind: Kind, ino: u64, attrs: Option<Attrs>) {
    put(out, u64::from(name));
    // The kind, and whether attributes follow.
    put(out, kind_code(kind) | (u64::from(attrs.is_some()) << 2));
    put(out, ino);
    if let Some(a) = attrs {
        put(out, a.size);
        put(out, a.alloc);
        put_time(out, a.mtime);
        put_time(out, a.ctime);
        put(out, u64::from(a.links));
    }
}

fn take_item(bytes: &[u8], at: &mut usize) -> Item {
    let name = take(bytes, at) as NameId;
    let code = take(bytes, at);
    let ino = take(bytes, at);
    let attrs = (code & 4 != 0).then(|| Attrs {
        size: take(bytes, at),
        alloc: take(bytes, at),
        mtime: take_time(bytes, at),
        ctime: take_time(bytes, at),
        links: u32::try_from(take(bytes, at)).unwrap_or(u32::MAX),
    });
    Item {
        name,
        kind: kind_of(code & 3),
        ino,
        attrs,
        child: None,
    }
}

impl Packer {
    /// Pack the folder a crawl handed with `slot`: its listing can be let
    /// go of once this returns.
    pub fn add(&mut self, slot: usize, crawled: &Crawled) {
        let name = if slot == 0 {
            0
        } else {
            self.names.intern(&crate::build::last_name(crawled))
        };
        if self.places.len() <= slot {
            self.places.resize(slot + 1, UNSEEN);
        }
        if crawled.watch != Watch::Unasked {
            self.watches.push((slot as FolderId, crawled.watch));
        }
        self.places[slot] = Place {
            at: self.packed.len(),
            parent: crawled.parent.map_or(NONE, |p| p as u32),
            name,
        };
        put_facts(&mut self.packed, crawled.stat.as_ref().map(facts));
        match &crawled.outcome {
            Outcome::Listed(listing) => {
                put_state(
                    &mut self.packed,
                    State::Listed {
                        again: listing.again,
                    },
                );
                self.order.clear();
                self.order.extend(0..listing.len() as u32);
                self.order.sort_unstable_by(|&a, &b| {
                    listing.name(a as usize).cmp(listing.name(b as usize))
                });
                put(&mut self.packed, listing.len() as u64);
                for &i in &self.order {
                    let entry = listing.entry(i as usize);
                    let name = self.names.intern(listing.name(i as usize));
                    put_item(
                        &mut self.packed,
                        name,
                        entry.kind,
                        entry.id.ino,
                        entry.attrs,
                    );
                }
                self.weight += listing.len() + 1;
            }
            Outcome::NotCovered(why) => {
                put_state(&mut self.packed, State::NotCovered(*why));
                put(&mut self.packed, 0);
                self.weight += 1;
            }
        }
    }

    /// The index of every folder packed: slot 0 is the root's, and every
    /// slot below the highest was handed.
    pub fn finish(self, limits: Limits) -> Index {
        self.finish_watched(limits).0
    }

    /// [`Packer::finish`], and what came of watching each folder packed,
    /// where a watch was asked.
    pub fn finish_watched(self, limits: Limits) -> (Index, Vec<(FolderId, Watch)>) {
        let Packer {
            names,
            packed,
            places,
            weight,
            watches,
            ..
        } = self;
        // The names first, so that their table is let go of before the
        // chunks are made.
        let names =
            Segment::from_bytes(Bytes::Owned(names.finish().into_boxed_slice()), true, None)
                .expect("names just written read back");
        let kids = Kids::of(&places);
        let chunks = Chunks::new(weight, limits);
        let chunks = preorder(&packed, &places, &kids, chunks);
        drop((packed, places, kids));
        let chunks = chunks.finish();
        let tree = Tree::from_chunks(
            chunks
                .into_iter()
                .enumerate()
                .map(|(id, c)| (id as u32, Arc::new(c)))
                .collect(),
            limits,
        )
        .expect("blocks unpacked in pre-order make a tree");
        (Index::new(tree, Names::new(names)), watches)
    }
}

/// Each folder's folders, by slot, in one table.
struct Kids {
    /// Where each slot's folders start in `slots`; one past the last.
    start: Vec<u32>,
    slots: Vec<u32>,
}

impl Kids {
    fn of(places: &[Place]) -> Kids {
        let mut start = vec![0u32; places.len() + 1];
        let held = |p: &Place| p.at != usize::MAX && p.parent != NONE;
        for p in places.iter().filter(|p| held(p)) {
            start[p.parent as usize + 1] += 1;
        }
        for i in 1..start.len() {
            start[i] += start[i - 1];
        }
        let mut fill = start.clone();
        let mut slots = vec![0u32; start[places.len()] as usize];
        for (slot, p) in places.iter().enumerate().filter(|(_, p)| held(p)) {
            let at = &mut fill[p.parent as usize];
            slots[*at as usize] = slot as u32;
            *at += 1;
        }
        Kids { start, slots }
    }

    fn of_slot(&self, slot: usize) -> &[u32] {
        &self.slots[self.start[slot] as usize..self.start[slot + 1] as usize]
    }
}

/// Every block unpacked in pre-order, a folder's folders in the order of
/// their names, as `blocks_of` lays them out.
fn preorder(packed: &[u8], places: &[Place], kids: &Kids, mut chunks: Chunks) -> Chunks {
    let mut by_name: HashMap<NameId, u32> = HashMap::new();
    let mut below: Vec<u32> = Vec::new();
    let mut stack: Vec<(u32, u32)> = vec![(0, 0)];
    while let Some((slot, depth)) = stack.pop() {
        let place = places[slot as usize];
        let mut at = place.at;
        let facts = take_facts(packed, &mut at);
        let state = take_state(packed, &mut at);
        let count = take(packed, &mut at) as usize;
        by_name.clear();
        by_name.extend(
            kids.of_slot(slot as usize)
                .iter()
                .map(|&k| (places[k as usize].name, k)),
        );
        below.clear();
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let mut item = take_item(packed, &mut at);
            if item.kind == Kind::Folder
                && let Some(&k) = by_name.get(&item.name)
            {
                item.child = Some(k as FolderId);
                below.push(k);
            }
            items.push(item);
        }
        stack.extend(below.iter().rev().map(|&k| (k, depth + 1)));
        chunks.push(Block {
            folder: slot as FolderId,
            parent: (slot != 0).then_some(place.parent as FolderId),
            name: place.name,
            depth,
            facts,
            state,
            items,
        });
    }
    chunks
}

/// Chunks made as blocks come in order, cut as [`Tree::build`] cuts them:
/// groups of about the same weight, none past `limits.target` unless a
/// single block is, then a group below `limits.min` joined to a
/// neighbour where both fit.
struct Chunks {
    each: usize,
    limits: Limits,
    made: Vec<(usize, Chunk)>,
    group: Vec<Block>,
    filled: usize,
}

impl Chunks {
    fn new(total: usize, limits: Limits) -> Chunks {
        let each = if total <= limits.max {
            total
        } else {
            total.div_ceil(total.div_ceil(limits.target.max(1)))
        };
        Chunks {
            each,
            limits,
            made: Vec::new(),
            group: Vec::new(),
            filled: 0,
        }
    }

    fn push(&mut self, block: Block) {
        let w = block.items.len() + 1;
        if !self.group.is_empty() && self.filled + w > self.each {
            self.close();
        }
        self.filled += w;
        self.group.push(block);
    }

    fn close(&mut self) {
        let group = std::mem::take(&mut self.group);
        self.made.push((self.filled, Chunk::new(&group)));
        self.filled = 0;
    }

    fn finish(mut self) -> Vec<Chunk> {
        if !self.group.is_empty() {
            self.close();
        }
        let Chunks {
            limits, mut made, ..
        } = self;
        // A small chunk joins a neighbour where both fit, as `cut` does.
        let join = |a: &(usize, Chunk), b: &(usize, Chunk)| {
            let mut blocks = a.1.decode();
            blocks.extend(b.1.decode());
            (a.0 + b.0, Chunk::new(&blocks))
        };
        let mut i = 0;
        while i < made.len() {
            let w = made[i].0;
            let fits = |j: usize| w + made[j].0 <= limits.max;
            let small = |j: usize| w < limits.min || made[j].0 < limits.min;
            if i > 0 && small(i - 1) && fits(i - 1) {
                let joined = join(&made[i - 1], &made[i]);
                made.remove(i);
                made[i - 1] = joined;
                i -= 1;
            } else if i + 1 < made.len() && small(i + 1) && fits(i + 1) {
                let joined = join(&made[i], &made[i + 1]);
                made.remove(i + 1);
                made[i] = joined;
            } else {
                i += 1;
            }
        }
        made.into_iter().map(|(_, c)| c).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every block of `index` in order, its names spelled out: two
    /// indexes that number names apart compare alike.
    #[cfg(target_os = "linux")]
    fn spelled(index: &Index) -> Vec<String> {
        let names = index.names();
        index
            .tree()
            .chunks()
            .flat_map(|(_, c)| c.decode())
            .map(|b| {
                let items: Vec<_> = b
                    .items
                    .iter()
                    .map(|i| {
                        let name = String::from_utf8_lossy(names.get(i.name));
                        (name.into_owned(), i.kind, i.ino, i.attrs, i.child)
                    })
                    .collect();
                format!(
                    "{} {:?} {} {} {:?} {:?} {:?}",
                    b.folder,
                    b.parent,
                    String::from_utf8_lossy(names.get(b.name)),
                    b.depth,
                    b.facts,
                    b.state,
                    items
                )
            })
            .collect()
    }

    /// Packed, then unpacked, a crawl's blocks are the blocks
    /// [`crate::build::blocks_of`] lays out, cut into the chunks
    /// [`Tree::build`] cuts: folders of every size, one past a chunk's
    /// most, one empty, one excluded.
    #[cfg(target_os = "linux")]
    #[test]
    fn packed_blocks_are_the_blocks_a_build_lays_out() {
        use skwird_crawl::{Cancel, Exclusions, Options, Root, crawl_all};
        use skwird_testkit::Scratch;

        use crate::build::{Top, blocks_of};

        let s = Scratch::new("packer-alike");
        for i in 0..30 {
            for j in 0..i * 3 {
                s.file(format!("f{i:02}/n{j} é.txt"));
            }
        }
        for j in 0..130 {
            s.file(format!("big/item {j}"));
        }
        s.dir("empty");
        s.file("kept out/secret");
        s.file("f03/deeper/still/x");
        let (ex, _) = Exclusions::resolve([s.join("kept out")]);
        let root = Root::open(s.path(), ex).unwrap();
        let opts = Options {
            threads: 3,
            ..Options::default()
        };
        let limits = Limits {
            target: 40,
            max: 60,
            min: 15,
        };
        let crawl = crawl_all(&root, &opts, &Cancel::default()).unwrap();
        let mut names = crate::names::Builder::new();
        let blocks = blocks_of(
            crawl.folders.clone(),
            Top {
                parent: None,
                name: 0,
                depth: 0,
            },
            &mut |slot| slot as FolderId,
            &mut |name| names.intern(name),
        );
        let laid = Index::new(
            Tree::build(blocks, limits),
            Names::new(
                Segment::from_bytes(Bytes::Owned(names.finish().into_boxed_slice()), true, None)
                    .unwrap(),
            ),
        );
        let packed = crate::build::build(crawl, limits);
        assert_eq!(spelled(&packed), spelled(&laid));
        let weights =
            |i: &Index| -> Vec<usize> { i.tree().chunks().map(|(_, c)| c.items()).collect() };
        assert_eq!(weights(&packed), weights(&laid));
        assert!(weights(&laid).len() > 5, "{:?}", weights(&laid));
    }

    #[test]
    fn numbers_read_back_as_packed() {
        let mut out = Vec::new();
        let values = [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX];
        for v in values {
            put(&mut out, v);
        }
        let times = [i64::MIN, -1, 0, 1, 1_763_000_000, i64::MAX];
        for t in times {
            put(&mut out, zig(t));
        }
        let mut at = 0;
        for v in values {
            assert_eq!(take(&out, &mut at), v);
        }
        for t in times {
            assert_eq!(unzig(take(&out, &mut at)), t);
        }
        assert_eq!(at, out.len());
    }

    /// Names whose hashes are alike are still two names.
    #[test]
    fn names_alike_in_hash_are_told_apart() {
        let mut names = Interner::new();
        let a = names.intern(b"alpha");
        // A name made to sit where "alpha" sits, as a colliding hash would.
        let b = names.ends.len() as NameId;
        names.bytes.extend_from_slice(b"beta");
        names.ends.push(names.bytes.len());
        names.by_hash.insert(hash_of(b"gamma"), b);
        assert_eq!(names.intern(b"alpha"), a);
        let c = names.intern(b"gamma");
        assert_ne!(c, b, "gamma's hash leads to beta, which is not gamma");
        assert_eq!(names.intern(b"gamma"), c);
        assert_eq!(Interner::get(&names.bytes, &names.ends, c), b"gamma");
    }
}
