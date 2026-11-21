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
use std::ops::Range;

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
#[derive(Default)]
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
        let count = self.list.len();
        let total: usize = self.list.iter().map(|n| n.len()).sum();
        let mut out = Vec::with_capacity(16 + count * 16 + total);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(count as u64).to_le_bytes());
        let mut end = 0u64;
        for name in &self.list {
            end += name.len() as u64;
            out.extend_from_slice(&end.to_le_bytes());
        }
        for name in &self.list {
            out.extend_from_slice(&mask(&lowered(name)).to_le_bytes());
        }
        for name in &self.list {
            out.extend_from_slice(name);
        }
        out
    }
}

/// The names, read in place.
#[derive(Debug)]
pub struct Names {
    bytes: Bytes,
    count: usize,
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default())
}

impl Names {
    /// The names in `bytes`, refused if they do not read as a names file.
    pub fn from_bytes(bytes: Bytes) -> Result<Names, Malformed> {
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
        if end != (bytes.len() - head) as u64 || word(&bytes, 16) != 0 {
            return Err(Malformed);
        }
        Ok(Names { bytes, count })
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

    fn span(&self, id: NameId) -> Range<usize> {
        let i = id as usize;
        let start = if i == 0 {
            0
        } else {
            word(&self.bytes, 16 + (i - 1) * 8) as usize
        };
        let end = word(&self.bytes, 16 + i * 8) as usize;
        self.head() + start..self.head() + end
    }

    /// The name's bytes, as the filesystem holds them.
    pub fn get(&self, id: NameId) -> &[u8] {
        &self.bytes[self.span(id)]
    }

    pub fn mask(&self, id: NameId) -> u64 {
        word(&self.bytes, 16 + self.count * 8 + id as usize * 8)
    }

    /// Every name's bytes and size, for the index's size.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
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
        let names = Names::from_bytes(Bytes::Owned(b.finish().into_boxed_slice())).unwrap();
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

    #[test]
    fn a_damaged_names_file_is_refused() {
        let mut b = Builder::new();
        b.intern(b"one");
        b.intern(b"two");
        let bytes = b.finish();
        let owned = |b: &[u8]| Bytes::Owned(b.to_vec().into_boxed_slice());
        for cut in [0, 15, 20, bytes.len() - 1] {
            assert!(
                Names::from_bytes(owned(&bytes[..cut])).is_err(),
                "cut at {cut}"
            );
        }
    }
}
