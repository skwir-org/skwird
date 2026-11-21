//! The index's names: each distinct name stored once (`docs/DESIGN.md`,
//! decision 8), so a query scores names, not entries, and with each a
//! mask of what it holds, so that most names are passed over before
//! their bytes are read.
//!
//! A names file, little-endian: `SKWN`, the version (4 bytes), the count
//! (8), each name's end in the bytes (8 each), each name's mask (8 each),
//! then the names' bytes, as the filesystem holds them. Name 0 is the
//! empty name, the root's.

use std::collections::HashMap;
use std::sync::Arc;

use crate::block::NameId;
use crate::chunk::Bytes;
use crate::packed::Malformed;

const MAGIC: &[u8; 4] = b"SKWN";
pub const VERSION: u32 = 1;

/// What a name holds, a bit for each: the letters `a` to `z`, the digits,
/// a space, `.`, `-`, `_`, any other ASCII character, any other
/// character. Taken from the name lowercased as skwir lowercases it
/// (`str::to_lowercase` of its lossy UTF-8 form).
pub fn mask(lowered: &str) -> u64 {
    let mut m = 0u64;
    for c in lowered.chars() {
        m |= 1 << bit(c);
    }
    m
}

/// A character's bit in a [`mask`].
fn bit(c: char) -> u32 {
    match c {
        'a'..='z' => c as u32 - 'a' as u32,
        '0'..='9' => 26 + (c as u32 - '0' as u32),
        ' ' => 36,
        '.' => 37,
        '-' => 38,
        '_' => 39,
        c if c.is_ascii() => 40,
        _ => NOT_ASCII,
    }
}

/// The bit of any character that is not ASCII.
pub const NOT_ASCII: u32 = 41;

/// A name lowercased as skwir lowercases it.
pub fn lowered(name: &[u8]) -> String {
    String::from_utf8_lossy(name).to_lowercase()
}

/// Names as they are given, each once, numbered in that order.
#[derive(Clone, Debug, Default)]
pub struct Builder {
    numbers: HashMap<Box<[u8]>, NameId>,
    list: Vec<Box<[u8]>>,
}

impl Builder {
    pub fn new() -> Builder {
        let mut b = Builder::default();
        b.intern(b"");
        b
    }

    /// The number of `name`, given now if it has none.
    pub fn intern(&mut self, name: &[u8]) -> NameId {
        if let Some(&n) = self.numbers.get(name) {
            return n;
        }
        let n = self.list.len() as NameId;
        self.numbers.insert(name.into(), n);
        self.list.push(name.into());
        n
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The names file.
    pub fn finish(self) -> Vec<u8> {
        segment_bytes(&self.list.iter().map(|n| &**n).collect::<Vec<_>>())
    }
}

/// The bytes of a names file of `names`, in that order, each kept as it is
/// (two alike are two names).
pub fn segment_bytes(names: &[&[u8]]) -> Vec<u8> {
    segment_of(names.len(), |i| names[i])
}

/// [`segment_bytes`] of the `count` names `name` gives, by number.
pub fn segment_of<'a>(count: usize, name: impl Fn(usize) -> &'a [u8]) -> Vec<u8> {
    let total: usize = (0..count).map(|i| name(i).len()).sum();
    let mut out = Vec::with_capacity(16 + count * 16 + total);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(count as u64).to_le_bytes());
    let mut end = 0u64;
    for i in 0..count {
        end += name(i).len() as u64;
        out.extend_from_slice(&end.to_le_bytes());
    }
    for i in 0..count {
        out.extend_from_slice(&mask(&lowered(name(i))).to_le_bytes());
    }
    for i in 0..count {
        out.extend_from_slice(name(i));
    }
    out
}

/// A names file, read in place: a run of names numbered from where the
/// one before ends.
#[derive(Debug)]
pub struct Segment {
    bytes: Bytes,
    count: usize,
    /// The save that wrote it, which names its file: none for one not yet
    /// saved.
    file: Option<u64>,
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default())
}

impl Segment {
    /// The names in `bytes`, refused if they do not read as a names file.
    /// The first segment's first name is the empty one, the root's.
    pub fn from_bytes(bytes: Bytes, first: bool, file: Option<u64>) -> Result<Segment, Malformed> {
        if bytes.len() < 16 || &bytes[..4] != MAGIC || bytes[4..8] != VERSION.to_le_bytes() {
            return Err(Malformed);
        }
        let count = usize::try_from(word(&bytes, 8)).map_err(|_| Malformed)?;
        let head = count
            .checked_mul(16)
            .and_then(|n| n.checked_add(16))
            .ok_or(Malformed)?;
        if head > bytes.len() || count == 0 {
            return Err(Malformed);
        }
        let mut end = 0;
        for i in 0..count {
            let next = word(&bytes, 16 + i * 8);
            if next < end {
                return Err(Malformed);
            }
            end = next;
        }
        if end != (bytes.len() - head) as u64 || (first && word(&bytes, 16) != 0) {
            return Err(Malformed);
        }
        Ok(Segment { bytes, count, file })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn head(&self) -> usize {
        16 + self.count * 16
    }

    /// Its `i`th name's bytes.
    pub fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 {
            0
        } else {
            word(&self.bytes, 16 + (i - 1) * 8) as usize
        };
        let end = word(&self.bytes, 16 + i * 8) as usize;
        &self.bytes[self.head() + start..self.head() + end]
    }

    pub fn mask(&self, i: usize) -> u64 {
        word(&self.bytes, 16 + self.count * 8 + i * 8)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn file(&self) -> Option<u64> {
        self.file
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self.bytes, Bytes::Mapped(_))
    }
}

/// Names given since the last save, in memory.
#[derive(Clone, Debug, Default)]
struct Pending {
    builder: Builder,
}

/// Every name of an index: its saved segments, then the names given
/// since. Names given since are told apart from each other, not from the
/// saved ones: a name a folder already held keeps its number when the
/// folder is read again, so only names new to their folder are given.
/// Cloned for a snapshot, it shares every segment.
#[derive(Clone, Debug)]
pub struct Names {
    segments: Vec<Arc<Segment>>,
    /// Each segment's first number.
    starts: Vec<NameId>,
    pending: Arc<Pending>,
}

impl Names {
    /// The names of a first segment.
    pub fn new(first: Segment) -> Names {
        Names::from_segments(vec![first])
    }

    pub fn from_segments(segments: Vec<Segment>) -> Names {
        let mut starts = Vec::with_capacity(segments.len());
        let mut at = 0;
        for s in &segments {
            starts.push(at);
            at += s.len() as NameId;
        }
        Names {
            segments: segments.into_iter().map(Arc::new).collect(),
            starts,
            pending: Arc::new(Pending {
                builder: Builder::default(),
            }),
        }
    }

    fn saved(&self) -> usize {
        self.segments
            .last()
            .map_or(0, |s| *self.starts.last().unwrap_or(&0) as usize + s.len())
    }

    pub fn len(&self) -> usize {
        self.saved() + self.pending.builder.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The segment holding `id`, and its place in it; none for a name
    /// given since the last save.
    fn place(&self, id: NameId) -> Option<(usize, usize)> {
        if id as usize >= self.saved() {
            return None;
        }
        let s = self.starts.partition_point(|&start| start <= id) - 1;
        Some((s, (id - self.starts[s]) as usize))
    }

    /// The name's bytes, as the filesystem holds them.
    pub fn get(&self, id: NameId) -> &[u8] {
        match self.place(id) {
            Some((s, i)) => self.segments[s].get(i),
            None => &self.pending.builder.list[id as usize - self.saved()],
        }
    }

    pub fn mask(&self, id: NameId) -> u64 {
        match self.place(id) {
            Some((s, i)) => self.segments[s].mask(i),
            None => mask(&lowered(self.get(id))),
        }
    }

    /// A number for `name`: the one given since the last save, or a new
    /// one.
    pub fn intern(&mut self, name: &[u8]) -> NameId {
        let saved = self.saved() as NameId;
        if let Some(&n) = self.pending.builder.numbers.get(name) {
            return saved + n;
        }
        saved + Arc::make_mut(&mut self.pending).builder.intern(name)
    }

    pub fn segments(&self) -> &[Arc<Segment>] {
        &self.segments
    }

    /// The names given since the last save, as a segment's bytes, or none.
    pub fn pending_bytes(&self) -> Option<Vec<u8>> {
        (!self.pending.builder.is_empty()).then(|| self.pending.builder.clone().finish())
    }

    /// Put `segments` in place of the saved ones and what was pending,
    /// the same names read from their files.
    pub fn sealed(&mut self, segments: Vec<Segment>) {
        let keep = std::mem::replace(self, Names::from_segments(segments));
        debug_assert_eq!(self.len(), keep.len());
    }

    /// The bytes every name takes, saved or not.
    pub fn bytes_len(&self) -> u64 {
        let pending: usize = self.pending.builder.list.iter().map(|n| n.len() + 16).sum();
        self.segments
            .iter()
            .map(|s| s.bytes().len() as u64)
            .sum::<u64>()
            + pending as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_back_once_each_with_their_masks() {
        let mut b = Builder::new();
        let a = b.intern("Report 2026.PDF".as_bytes());
        let c = b.intern(b"\xffnot utf-8");
        assert_eq!(b.intern("Report 2026.PDF".as_bytes()), a);
        assert_eq!((a, c), (1, 2));
        let names = Names::new(
            Segment::from_bytes(Bytes::Owned(b.finish().into_boxed_slice()), true, None).unwrap(),
        );
        assert_eq!(names.len(), 3);
        assert_eq!(names.get(0), b"");
        assert_eq!(names.get(a), "Report 2026.PDF".as_bytes());
        assert_eq!(names.get(c), b"\xffnot utf-8");
        let m = names.mask(a);
        for ch in ['r', 'e', 'p', 'o', 't', '2', '0', '6', ' ', '.', 'd', 'f'] {
            assert_ne!(m & (1 << bit(ch)), 0, "{ch}");
        }
        assert_eq!(m & (1 << bit('z')), 0);
        assert_ne!(
            names.mask(c) & (1 << NOT_ASCII),
            0,
            "the replacement character"
        );
    }

    /// Names given after a save follow the saved ones; a clone taken
    /// before keeps what it had.
    #[test]
    fn names_given_since_follow_the_saved_and_a_snapshot_keeps_its_own() {
        let mut b = Builder::new();
        b.intern(b"one");
        let mut names = Names::new(
            Segment::from_bytes(Bytes::Owned(b.finish().into_boxed_slice()), true, None).unwrap(),
        );
        let before = names.clone();
        let two = names.intern(b"two");
        assert_eq!(two, 2);
        assert_eq!(names.intern(b"two"), 2);
        assert_eq!(names.get(two), b"two");
        assert_eq!(names.mask(two), mask("two"));
        assert_eq!(before.len(), 2);
        let pending = names.pending_bytes().unwrap();
        let second =
            Segment::from_bytes(Bytes::Owned(pending.into_boxed_slice()), false, Some(1)).unwrap();
        let first = Segment::from_bytes(
            Bytes::Owned(names.segments()[0].bytes().to_vec().into_boxed_slice()),
            true,
            Some(0),
        )
        .unwrap();
        names.sealed(vec![first, second]);
        assert_eq!(names.len(), 3);
        assert_eq!(names.get(2), b"two");
        assert!(names.pending_bytes().is_none());
    }

    #[test]
    fn a_damaged_names_file_is_refused() {
        let mut b = Builder::new();
        b.intern(b"one");
        b.intern(b"two");
        let bytes = b.finish();
        let owned = |b: &[u8]| Bytes::Owned(b.to_vec().into_boxed_slice());
        for cut in [0, 15, 20, bytes.len() - 1] {
            assert!(
                Segment::from_bytes(owned(&bytes[..cut]), true, None).is_err(),
                "cut at {cut}"
            );
        }
    }
}
