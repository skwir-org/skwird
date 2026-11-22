//! A chunk: a run of folder blocks in pre-order, packed in columns
//! ([`crate::packed`]) and read in place, from memory or from its file
//! mapped.
//!
//! Its bytes, little-endian: `SKWC`, the layout's version (4 bytes), the
//! number of columns (4), each column's offset and the end (8 each), then
//! the columns. Seventeen columns hold one value a block, ten one an
//! entry, four one per zone of 1,024 entries (the largest size, the range
//! of modification times, the kinds present), so that a query on size,
//! time or kind passes over most of a chunk. A chunk whose bytes do not
//! read as this layout is refused whole: the index is a cache, rebuilt.

use std::ops::{Deref, Range};

use skwird_crawl::{Attrs, FileId, Kind, Time, Why};

use crate::block::{Block, Facts, FolderId, Item, State};
use crate::packed::{Layout, Malformed, Packed, from_signed, pack, to_signed};

const MAGIC: &[u8; 4] = b"SKWC";
/// The layout's version: a change to the columns changes it.
pub const VERSION: u32 = 1;
/// Entries a zone sums up.
pub const ZONE: usize = 1024;

// Columns with a value a block.
const B_FOLDER: usize = 0;
const B_PARENT: usize = 1; // the parent's number and one, 0 for none
const B_NAME: usize = 2;
const B_DEPTH: usize = 3;
const B_END: usize = 4; // where its entries end, counted over the chunk
const B_STATE: usize = 5;
const B_ERRNO: usize = 6;
const B_FACTS: usize = 7; // 1 where a stat gave its facts
const B_DEV: usize = 8;
const B_INO: usize = 9;
const B_CTIME: usize = 10;
const B_CTIME_NS: usize = 11;
const B_MTIME: usize = 12;
const B_MTIME_NS: usize = 13;
const B_OWNER: usize = 14;
const B_GROUP: usize = 15;
const B_MODE: usize = 16;
// Columns with a value an entry.
const I_NAME: usize = 17;
const I_KIND: usize = 18; // the kind, and 4 where it has attributes
const I_INO: usize = 19;
const I_SIZE: usize = 20;
const I_ALLOC: usize = 21;
const I_MTIME: usize = 22;
const I_MTIME_NS: usize = 23;
const I_CTIME: usize = 24;
const I_CTIME_NS: usize = 25;
const I_CHILD: usize = 26; // the folder's number and one, 0 for none
// Columns with a value a zone.
const Z_SIZE: usize = 27; // the largest size
const Z_MTIME_MIN: usize = 28;
const Z_MTIME_MAX: usize = 29;
const Z_KINDS: usize = 30; // a bit for each kind present
const COLUMNS: usize = 31;

/// A chunk's bytes: its own, or its file's, mapped.
pub enum Bytes {
    Owned(Box<[u8]>),
    Mapped(crate::map::Map),
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Owned(b) => b,
            Bytes::Mapped(m) => m,
        }
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes", self.len())
    }
}

#[derive(Debug)]
pub struct Chunk {
    bytes: Bytes,
    columns: Vec<Layout>,
    blocks: usize,
    items: usize,
}

pub fn kind_code(kind: Kind) -> u64 {
    match kind {
        Kind::File => 0,
        Kind::Folder => 1,
        Kind::Link => 2,
        Kind::Other => 3,
    }
}

pub(crate) fn kind_of(code: u64) -> Kind {
    match code & 3 {
        0 => Kind::File,
        1 => Kind::Folder,
        2 => Kind::Link,
        _ => Kind::Other,
    }
}

pub(crate) fn state_code(state: State) -> (u64, u64) {
    match state {
        State::Listed { again: false } => (0, 0),
        State::Listed { again: true } => (1, 0),
        State::NotCovered(why) => match why {
            Why::Excluded => (2, 0),
            Why::Private => (3, 0),
            Why::Denied => (4, 0),
            Why::Failed(e) => (5, u64::from(e.unsigned_abs())),
            Why::CutShort(e) => (6, u64::from(e.unsigned_abs())),
            Why::Gone => (7, 0),
            Why::Replaced => (8, 0),
            Why::Mount => (9, 0),
            Why::NetworkMount => (10, 0),
            Why::Snapshot => (12, 0),
            Why::Unread => (11, 0),
        },
    }
}

pub(crate) fn state_of(code: u64, errno: u64) -> Option<State> {
    let errno = i32::try_from(errno).unwrap_or(i32::MAX);
    Some(match code {
        0 => State::Listed { again: false },
        1 => State::Listed { again: true },
        2 => State::NotCovered(Why::Excluded),
        3 => State::NotCovered(Why::Private),
        4 => State::NotCovered(Why::Denied),
        5 => State::NotCovered(Why::Failed(errno)),
        6 => State::NotCovered(Why::CutShort(errno)),
        7 => State::NotCovered(Why::Gone),
        8 => State::NotCovered(Why::Replaced),
        9 => State::NotCovered(Why::Mount),
        10 => State::NotCovered(Why::NetworkMount),
        11 => State::NotCovered(Why::Unread),
        12 => State::NotCovered(Why::Snapshot),
        _ => return None,
    })
}

/// The bytes of a chunk of `blocks`.
pub fn encode(blocks: &[Block]) -> Vec<u8> {
    let mut cols: Vec<Vec<u64>> = vec![Vec::new(); COLUMNS];
    let mut end = 0u64;
    for b in blocks {
        let (state, errno) = state_code(b.state);
        let facts = b.facts.unwrap_or(Facts {
            id: FileId::default(),
            ctime: Time::default(),
            mtime: Time::default(),
            owner: 0,
            group: 0,
            mode: 0,
        });
        end += b.items.len() as u64;
        for (c, v) in [
            (B_FOLDER, u64::from(b.folder)),
            (B_PARENT, b.parent.map_or(0, |p| u64::from(p) + 1)),
            (B_NAME, u64::from(b.name)),
            (B_DEPTH, u64::from(b.depth)),
            (B_END, end),
            (B_STATE, state),
            (B_ERRNO, errno),
            (B_FACTS, u64::from(b.facts.is_some())),
            (B_DEV, facts.id.dev),
            (B_INO, facts.id.ino),
            (B_CTIME, from_signed(facts.ctime.sec)),
            (B_CTIME_NS, u64::from(facts.ctime.nsec)),
            (B_MTIME, from_signed(facts.mtime.sec)),
            (B_MTIME_NS, u64::from(facts.mtime.nsec)),
            (B_OWNER, u64::from(facts.owner)),
            (B_GROUP, u64::from(facts.group)),
            (B_MODE, u64::from(facts.mode)),
        ] {
            cols[c].push(v);
        }
        for item in &b.items {
            let attrs = item.attrs.unwrap_or(Attrs {
                size: 0,
                alloc: 0,
                mtime: Time::default(),
                ctime: Time::default(),
            });
            let kind = kind_code(item.kind) | if item.attrs.is_some() { 4 } else { 0 };
            for (c, v) in [
                (I_NAME, u64::from(item.name)),
                (I_KIND, kind),
                (I_INO, item.ino),
                (I_SIZE, attrs.size),
                (I_ALLOC, attrs.alloc),
                (I_MTIME, from_signed(attrs.mtime.sec)),
                (I_MTIME_NS, u64::from(attrs.mtime.nsec)),
                (I_CTIME, from_signed(attrs.ctime.sec)),
                (I_CTIME_NS, u64::from(attrs.ctime.nsec)),
                (I_CHILD, item.child.map_or(0, |c| u64::from(c) + 1)),
            ] {
                cols[c].push(v);
            }
        }
    }
    let items = cols[I_NAME].len();
    for zone in 0..items.div_ceil(ZONE) {
        let range = zone * ZONE..((zone + 1) * ZONE).min(items);
        let mut size = 0;
        let (mut tmin, mut tmax) = (u64::MAX, 0);
        let mut kinds = 0;
        for i in range {
            kinds |= 1 << (cols[I_KIND][i] & 3);
            // An entry without attributes is no size and no time, so a
            // query on either passes over it; its zone says nothing of it.
            if cols[I_KIND][i] & 4 != 0 {
                size = size.max(cols[I_SIZE][i]);
                tmin = tmin.min(cols[I_MTIME][i]);
                tmax = tmax.max(cols[I_MTIME][i]);
            }
        }
        cols[Z_SIZE].push(size);
        cols[Z_MTIME_MIN].push(tmin);
        cols[Z_MTIME_MAX].push(tmax);
        cols[Z_KINDS].push(kinds);
    }
    let packed: Vec<Vec<u8>> = cols.iter().map(|c| pack(c)).collect();
    let head = 4 + 4 + 4 + 8 * (COLUMNS + 1);
    let mut out = Vec::with_capacity(head + packed.iter().map(Vec::len).sum::<usize>());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(COLUMNS as u32).to_le_bytes());
    let mut at = head as u64;
    for col in &packed {
        out.extend_from_slice(&at.to_le_bytes());
        at += col.len() as u64;
    }
    out.extend_from_slice(&at.to_le_bytes());
    for col in packed {
        out.extend_from_slice(&col);
    }
    out
}

impl Chunk {
    /// A chunk of `blocks`, in memory.
    pub fn new(blocks: &[Block]) -> Chunk {
        Chunk::from_bytes(Bytes::Owned(encode(blocks).into_boxed_slice()))
            .expect("a chunk just encoded reads back")
    }

    /// The chunk in `bytes`, refused if they do not read as its layout.
    pub fn from_bytes(bytes: Bytes) -> Result<Chunk, Malformed> {
        let head = bytes.get(..12).ok_or(Malformed)?;
        let word = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap_or_default());
        if &head[..4] != MAGIC || word(4) != VERSION || word(8) as usize != COLUMNS {
            return Err(Malformed);
        }
        let offsets: Vec<usize> = (0..=COLUMNS)
            .map(|c| {
                let at = 12 + c * 8;
                let raw = bytes.get(at..at + 8).ok_or(Malformed)?;
                usize::try_from(u64::from_le_bytes(raw.try_into().map_err(|_| Malformed)?))
                    .map_err(|_| Malformed)
            })
            .collect::<Result<_, _>>()?;
        if offsets[COLUMNS] != bytes.len() || offsets.windows(2).any(|w| w[0] > w[1]) {
            return Err(Malformed);
        }
        // Each layout counted from the chunk's first byte.
        let columns: Vec<Layout> = offsets
            .windows(2)
            .map(|w| Layout::read(&bytes[w[0]..w[1]]).map(|l| l.shifted(w[0])))
            .collect::<Result<_, _>>()?;
        let blocks = columns[B_FOLDER].len();
        let items = columns[I_NAME].len();
        let sizes_fit = (0..COLUMNS).all(|c| {
            let want = match c {
                c if c < I_NAME => blocks,
                c if c < Z_SIZE => items,
                _ => items.div_ceil(ZONE),
            };
            columns[c].len() == want
        });
        if !sizes_fit {
            return Err(Malformed);
        }
        let chunk = Chunk {
            bytes,
            columns,
            blocks,
            items,
        };
        // Each block's entries after the one before, the last ending the
        // chunk's, and every state one this layout knows.
        let view = chunk.view();
        let mut end = 0;
        for b in 0..blocks {
            let next = view.col(B_END).get(b);
            if next < end || state_of(view.col(B_STATE).get(b), 0).is_none() {
                return Err(Malformed);
            }
            end = next;
        }
        if end != items as u64 {
            return Err(Malformed);
        }
        Ok(chunk)
    }

    /// Every column, read in place.
    pub fn view(&self) -> View<'_> {
        View {
            cols: std::array::from_fn(|c| Packed::new(&self.bytes, &self.columns[c])),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether it is read from its file, mapped.
    pub fn is_mapped(&self) -> bool {
        matches!(self.bytes, Bytes::Mapped(_))
    }

    /// Its blocks.
    pub fn len(&self) -> usize {
        self.blocks
    }

    pub fn is_empty(&self) -> bool {
        self.blocks == 0
    }

    /// Its weight: its entries, and one for each block.
    pub fn items(&self) -> usize {
        self.items + self.blocks
    }

    /// Its entries alone.
    pub fn entries(&self) -> usize {
        self.items
    }

    pub fn depth(&self, i: usize) -> u32 {
        self.view().depth(i)
    }

    pub fn folder(&self, i: usize) -> FolderId {
        self.view().folder(i)
    }

    pub fn block(&self, i: usize) -> Block {
        self.view().block(i)
    }

    /// Its blocks, as they are to be rewritten.
    pub fn decode(&self) -> Vec<Block> {
        let view = self.view();
        (0..self.blocks).map(|b| view.block(b)).collect()
    }
}

/// A chunk's columns, read in place.
pub struct View<'a> {
    cols: [Packed<'a>; COLUMNS],
}

impl View<'_> {
    fn col(&self, c: usize) -> Packed<'_> {
        self.cols[c]
    }

    fn time(&self, sec: usize, nsec: usize, i: usize) -> Time {
        Time {
            sec: to_signed(self.cols[sec].get(i)),
            nsec: self.cols[nsec].get(i) as u32,
        }
    }

    pub fn folder(&self, b: usize) -> FolderId {
        self.cols[B_FOLDER].get(b) as FolderId
    }

    pub fn depth(&self, b: usize) -> u32 {
        self.cols[B_DEPTH].get(b) as u32
    }

    pub fn parent(&self, b: usize) -> Option<FolderId> {
        self.cols[B_PARENT]
            .get(b)
            .checked_sub(1)
            .map(|p| p as FolderId)
    }

    pub fn name(&self, b: usize) -> u32 {
        self.cols[B_NAME].get(b) as u32
    }

    /// The entries of block `b`, by their place in the chunk.
    pub fn entries(&self, b: usize) -> Range<usize> {
        let start = if b == 0 {
            0
        } else {
            self.cols[B_END].get(b - 1) as usize
        };
        start..self.cols[B_END].get(b) as usize
    }

    /// The block holding entry `i`.
    pub fn block_of(&self, i: usize) -> usize {
        let ends = &self.cols[B_END];
        let (mut lo, mut hi) = (0, ends.len());
        // The first block whose entries end past `i`.
        while lo < hi {
            let mid = usize::midpoint(lo, hi);
            if ends.get(mid) as usize > i {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    pub fn state(&self, b: usize) -> State {
        state_of(self.cols[B_STATE].get(b), self.cols[B_ERRNO].get(b))
            .unwrap_or(State::NotCovered(Why::Failed(0)))
    }

    pub fn facts(&self, b: usize) -> Option<Facts> {
        (self.cols[B_FACTS].get(b) == 1).then(|| Facts {
            id: FileId {
                dev: self.cols[B_DEV].get(b),
                ino: self.cols[B_INO].get(b),
            },
            ctime: self.time(B_CTIME, B_CTIME_NS, b),
            mtime: self.time(B_MTIME, B_MTIME_NS, b),
            owner: self.cols[B_OWNER].get(b) as u32,
            group: self.cols[B_GROUP].get(b) as u32,
            mode: self.cols[B_MODE].get(b) as u16,
        })
    }

    /// The column of the entries' names, for a scan to decode by runs.
    pub fn item_names(&self) -> Packed<'_> {
        self.cols[I_NAME]
    }

    /// The column of the entries' kinds ([`kind_code`], and 4 where an
    /// entry has attributes).
    pub fn item_kinds(&self) -> Packed<'_> {
        self.cols[I_KIND]
    }

    pub fn item_name(&self, i: usize) -> u32 {
        self.cols[I_NAME].get(i) as u32
    }

    pub fn item_kind(&self, i: usize) -> Kind {
        kind_of(self.cols[I_KIND].get(i))
    }

    pub fn item_has_attrs(&self, i: usize) -> bool {
        self.cols[I_KIND].get(i) & 4 != 0
    }

    pub fn item_size(&self, i: usize) -> u64 {
        self.cols[I_SIZE].get(i)
    }

    /// An entry's modification time's seconds, in the order of
    /// [`crate::packed::from_signed`].
    pub fn item_mtime_key(&self, i: usize) -> u64 {
        self.cols[I_MTIME].get(i)
    }

    pub fn item_child(&self, i: usize) -> Option<FolderId> {
        self.cols[I_CHILD]
            .get(i)
            .checked_sub(1)
            .map(|c| c as FolderId)
    }

    pub fn item(&self, i: usize) -> Item {
        let has = self.item_has_attrs(i);
        Item {
            name: self.item_name(i),
            kind: self.item_kind(i),
            ino: self.cols[I_INO].get(i),
            attrs: has.then(|| Attrs {
                size: self.cols[I_SIZE].get(i),
                alloc: self.cols[I_ALLOC].get(i),
                mtime: self.time(I_MTIME, I_MTIME_NS, i),
                ctime: self.time(I_CTIME, I_CTIME_NS, i),
            }),
            child: self.item_child(i),
        }
    }

    /// What zone `z` sums up: the largest size, the least and the largest
    /// modification time (as [`View::item_mtime_key`]), the kinds present
    /// (a bit each, as [`kind_code`]).
    pub fn zone(&self, z: usize) -> (u64, u64, u64, u64) {
        (
            self.cols[Z_SIZE].get(z),
            self.cols[Z_MTIME_MIN].get(z),
            self.cols[Z_MTIME_MAX].get(z),
            self.cols[Z_KINDS].get(z),
        )
    }

    pub fn block(&self, b: usize) -> Block {
        Block {
            folder: self.folder(b),
            parent: self.parent(b),
            name: self.name(b),
            depth: self.depth(b),
            facts: self.facts(b),
            state: self.state(b),
            items: self.entries(b).map(|i| self.item(i)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Block> {
        let facts = Facts {
            id: FileId { dev: 42, ino: 256 },
            ctime: Time {
                sec: 1_790_000_000,
                nsec: 123,
            },
            mtime: Time {
                sec: -86_400,
                nsec: 999_999_999,
            },
            owner: 1000,
            group: 100,
            mode: 0o755,
        };
        let file = |name, size| Item {
            name,
            kind: Kind::File,
            ino: 1000 + u64::from(name),
            attrs: Some(Attrs {
                size,
                alloc: size / 2,
                mtime: Time {
                    sec: 1_700_000_000 + i64::from(name),
                    nsec: 5,
                },
                ctime: Time {
                    sec: 1_700_000_100,
                    nsec: 0,
                },
            }),
            child: None,
        };
        vec![
            Block {
                folder: 0,
                parent: None,
                name: 0,
                depth: 0,
                facts: Some(facts),
                state: State::Listed { again: true },
                items: vec![
                    file(1, 10),
                    Item {
                        name: 2,
                        kind: Kind::Folder,
                        ino: 7,
                        attrs: None,
                        child: Some(1),
                    },
                    file(3, u64::MAX),
                ],
            },
            Block {
                folder: 1,
                parent: Some(0),
                name: 2,
                depth: 1,
                facts: None,
                state: State::NotCovered(Why::CutShort(5)),
                items: Vec::new(),
            },
            Block {
                folder: 9,
                parent: Some(1),
                name: 4,
                depth: 2,
                facts: Some(facts),
                state: State::Listed { again: false },
                items: (10..3000).map(|n| file(n, u64::from(n) * 3)).collect(),
            },
        ]
    }

    #[test]
    fn blocks_come_back_as_they_were_encoded() {
        let blocks = sample();
        let chunk = Chunk::new(&blocks);
        assert_eq!(chunk.decode(), blocks);
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.entries(), 3 + 2990);
        let view = chunk.view();
        assert_eq!(view.entries(2), 3..2993);
        assert_eq!(view.block_of(0), 0);
        assert_eq!(view.block_of(3), 2, "block 1 holds none");
        assert_eq!(view.block_of(2992), 2);
        let (largest, _, _, kinds) = view.zone(0);
        assert_eq!(largest, u64::MAX);
        assert_eq!(kinds, 0b11, "files and a folder");
    }

    #[test]
    fn bytes_of_another_layout_or_damaged_are_refused() {
        let bytes = encode(&sample());
        let owned = |b: &[u8]| Bytes::Owned(b.to_vec().into_boxed_slice());
        assert!(Chunk::from_bytes(owned(&bytes)).is_ok());
        let mut other = bytes.clone();
        other[4] = 99; // the version
        assert!(Chunk::from_bytes(owned(&other)).is_err());
        for cut in [0, 11, 100, bytes.len() - 1] {
            assert!(
                Chunk::from_bytes(owned(&bytes[..cut])).is_err(),
                "cut at {cut}"
            );
        }
        let mut wild = bytes.clone();
        let last = wild.len() - 20;
        wild[last] ^= 0xff;
        // A flipped bit may still read as a chunk, but never past its end.
        if let Ok(chunk) = Chunk::from_bytes(owned(&wild)) {
            let _ = chunk.decode();
        }
    }
}
