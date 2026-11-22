//! Integers packed in runs of 64 (`docs/DESIGN.md`, decision 8, from
//! fsearch 0.2): each run kept as its least value and every value's
//! distance from it, at the bits the largest distance needs. Inodes made
//! together, times of files written together and the numbers of the
//! names of one folder take few bits; a run of one value takes none.
//!
//! A column's bytes, little-endian:
//!
//! | field | size |
//! |---|---|
//! | values | 8 |
//! | each run's least value | 8 a run |
//! | each run's width in bits | 1 a run |
//! | each run's first bit | 8 a run |
//! | the bits | 8 a word |

use std::ops::Range;

/// Values a run holds.
pub const RUN: usize = 64;

/// Bytes of a column of `values`, packed.
pub fn pack(values: &[u64]) -> Vec<u8> {
    let runs = values.len().div_ceil(RUN);
    let mut bases = Vec::with_capacity(runs);
    let mut widths = Vec::with_capacity(runs);
    let mut starts = Vec::with_capacity(runs);
    let mut words: Vec<u64> = Vec::new();
    let mut bit: u64 = 0;
    for run in values.chunks(RUN) {
        let base = run.iter().copied().min().unwrap_or(0);
        let span = run.iter().map(|v| v - base).max().unwrap_or(0);
        let width = 64 - span.leading_zeros();
        bases.push(base);
        widths.push(width as u8);
        starts.push(bit);
        for &v in run {
            write_bits(&mut words, bit, width, v - base);
            bit += u64::from(width);
        }
    }
    // A word past the last bit, so a reader of a value that ends a word
    // never reads past the bytes.
    while words.len() < bit.div_ceil(64) as usize + 1 {
        words.push(0);
    }
    let mut out = Vec::with_capacity(8 + runs * 17 + words.len() * 8);
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for b in bases {
        out.extend_from_slice(&b.to_le_bytes());
    }
    out.extend_from_slice(&widths);
    for s in starts {
        out.extend_from_slice(&s.to_le_bytes());
    }
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

fn write_bits(words: &mut Vec<u64>, bit: u64, width: u32, value: u64) {
    if width == 0 {
        return;
    }
    let word = (bit / 64) as usize;
    let shift = (bit % 64) as u32;
    while words.len() < word + 2 {
        words.push(0);
    }
    words[word] |= value << shift;
    if shift + width > 64 {
        words[word + 1] |= value >> (64 - shift);
    }
}

/// Where a column's parts are in its bytes, checked once: a column is
/// then read in place without checks ([`Packed::new`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    len: usize,
    bases: Range<usize>,
    widths: Range<usize>,
    starts: Range<usize>,
    words: Range<usize>,
}

/// Why bytes are not a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

fn u64_at(bytes: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
    u64::from_le_bytes(b)
}

impl Layout {
    /// The layout of the column in `bytes`, its parts checked to fit: a
    /// damaged file is an error, never a read past its end.
    pub fn read(bytes: &[u8]) -> Result<Layout, Malformed> {
        let head = bytes.get(..8).ok_or(Malformed)?;
        let len = usize::try_from(u64::from_le_bytes(head.try_into().map_err(|_| Malformed)?))
            .map_err(|_| Malformed)?;
        let runs = len.div_ceil(RUN);
        let mut at = 8usize;
        let mut take = |n: usize| -> Result<Range<usize>, Malformed> {
            let end = at.checked_add(n).ok_or(Malformed)?;
            if end > bytes.len() {
                return Err(Malformed);
            }
            let part = at..end;
            at = end;
            Ok(part)
        };
        let bases = take(runs.checked_mul(8).ok_or(Malformed)?)?;
        let widths = take(runs)?;
        let starts = take(runs.checked_mul(8).ok_or(Malformed)?)?;
        let words = at..bytes.len();
        if words.len() % 8 != 0 {
            return Err(Malformed);
        }
        let word_count = (words.len() / 8) as u64;
        // Every run's bits within the words, so no read goes past them.
        for run in 0..runs {
            let width = u64::from(bytes[widths.start + run]);
            if width > 64 {
                return Err(Malformed);
            }
            let count = (len - run * RUN).min(RUN) as u64;
            let end = u64_at(&bytes[starts.clone()], run)
                .checked_add(width * count)
                .ok_or(Malformed)?;
            if width > 0 && end.div_ceil(64) + 1 > word_count {
                return Err(Malformed);
            }
        }
        Ok(Layout {
            len,
            bases,
            widths,
            starts,
            words,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The same layout, for bytes that hold the column `offset` bytes in.
    #[must_use]
    pub fn shifted(self, offset: usize) -> Layout {
        let by = |r: Range<usize>| r.start + offset..r.end + offset;
        Layout {
            len: self.len,
            bases: by(self.bases),
            widths: by(self.widths),
            starts: by(self.starts),
            words: by(self.words),
        }
    }
}

/// A packed column, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Packed<'a> {
    len: usize,
    bases: &'a [u8],
    widths: &'a [u8],
    starts: &'a [u8],
    words: &'a [u8],
}

impl<'a> Packed<'a> {
    /// The column in `bytes`, whose layout was read from them.
    pub fn new(bytes: &'a [u8], layout: &Layout) -> Packed<'a> {
        Packed {
            len: layout.len,
            bases: &bytes[layout.bases.clone()],
            widths: &bytes[layout.widths.clone()],
            starts: &bytes[layout.starts.clone()],
            words: &bytes[layout.words.clone()],
        }
    }

    /// The column in `bytes`, checked.
    pub fn read(bytes: &'a [u8]) -> Result<Packed<'a>, Malformed> {
        Ok(Packed::new(bytes, &Layout::read(bytes)?))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `i`th value.
    pub fn get(&self, i: usize) -> u64 {
        let run = i / RUN;
        let base = u64_at(self.bases, run);
        let width = u32::from(self.widths[run]);
        if width == 0 {
            return base;
        }
        let bit = u64_at(self.starts, run) + (i % RUN) as u64 * u64::from(width);
        let word = (bit / 64) as usize;
        let shift = (bit % 64) as u32;
        let mut v = u64_at(self.words, word) >> shift;
        if shift + width > 64 {
            v |= u64_at(self.words, word + 1) << (64 - shift);
        }
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        base + (v & mask)
    }

    /// The values of run `run` (the 64 from `run * 64`), into `out`; gives
    /// how many there are.
    pub fn decode_run(&self, run: usize, out: &mut [u64; RUN]) -> usize {
        let n = (self.len - run * RUN).min(RUN);
        let base = u64_at(self.bases, run);
        let width = u32::from(self.widths[run]);
        if width == 0 {
            out[..n].fill(base);
            return n;
        }
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let mut bit = u64_at(self.starts, run);
        for slot in out.iter_mut().take(n) {
            let word = (bit / 64) as usize;
            let shift = (bit % 64) as u32;
            let mut v = u64_at(self.words, word) >> shift;
            if shift + width > 64 {
                v |= u64_at(self.words, word + 1) << (64 - shift);
            }
            *slot = base + (v & mask);
            bit += u64::from(width);
        }
        n
    }

    /// The least and the largest value of the run holding `i`, for a scan
    /// to pass over a run that cannot match.
    pub fn run_bounds(&self, i: usize) -> (u64, u64) {
        let run = i / RUN;
        let base = u64_at(self.bases, run);
        let width = u32::from(self.widths[run]);
        let top = if width == 64 {
            u64::MAX
        } else {
            base.saturating_add((1u64 << width) - 1)
        };
        (base, top)
    }
}

/// The column `bytes` with each value `(i, value)` of `changes` set: in
/// place where every value fits its run (at least its least value, within
/// its width), so that a file's new size or time costs its own bits; the
/// column packed again otherwise. Either reads back alike.
pub fn patch(bytes: &[u8], changes: &[(usize, u64)]) -> Result<Vec<u8>, Malformed> {
    let layout = Layout::read(bytes)?;
    if changes.iter().any(|&(i, _)| i >= layout.len) {
        return Err(Malformed);
    }
    let run_of = |bytes: &[u8], i: usize| {
        let run = i / RUN;
        (
            run,
            u64_at(&bytes[layout.bases.clone()], run),
            u32::from(bytes[layout.widths.start + run]),
        )
    };
    let fits = changes.iter().all(|&(i, v)| {
        let (_, base, width) = run_of(bytes, i);
        v >= base && (width == 64 || (v - base) >> width == 0)
    });
    if !fits {
        let column = Packed::new(bytes, &layout);
        let mut values: Vec<u64> = (0..layout.len).map(|i| column.get(i)).collect();
        for &(i, v) in changes {
            values[i] = v;
        }
        return Ok(pack(&values));
    }
    let mut out = bytes.to_vec();
    for &(i, v) in changes {
        let (run, base, width) = run_of(&out, i);
        if width == 0 {
            continue; // the run's one value, which it fits
        }
        let bit = u64_at(&out[layout.starts.clone()], run) + (i % RUN) as u64 * u64::from(width);
        set_bits(&mut out[layout.words.clone()], bit, width, v - base);
    }
    Ok(out)
}

/// Write `value`'s `width` bits at `bit` of the little-endian `words`,
/// clearing what was there.
fn set_bits(words: &mut [u8], bit: u64, width: u32, value: u64) {
    let word = (bit / 64) as usize;
    let shift = (bit % 64) as u32;
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    let put = |words: &mut [u8], i: usize, w: u64| {
        words[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    };
    let low = (u64_at(words, word) & !(mask << shift)) | (value << shift);
    put(words, word, low);
    if shift + width > 64 {
        let written = 64 - shift;
        let high = (u64_at(words, word + 1) & !(mask >> written)) | (value >> written);
        put(words, word + 1, high);
    }
}

/// A signed value in the order of unsigned ones: times before 1970 pack
/// as well as times after.
pub fn from_signed(v: i64) -> u64 {
    (v as u64) ^ (1 << 63)
}

#[allow(clippy::cast_possible_wrap)] // the bits read back as they were, `from_signed`'s inverse
pub fn to_signed(v: u64) -> i64 {
    (v ^ (1 << 63)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[u64]) {
        let bytes = pack(values);
        let column = Packed::read(&bytes).unwrap();
        assert_eq!(column.len(), values.len());
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(column.get(i), v, "value {i}");
        }
    }

    #[test]
    fn values_come_back_as_they_were_packed() {
        round_trip(&[]);
        round_trip(&[5]);
        round_trip(&[7; 200]);
        round_trip(&(0..1000).collect::<Vec<_>>());
        round_trip(&[0, u64::MAX, 1, u64::MAX - 1, 12_345]);
        round_trip(
            &(0..300)
                .map(|i: u64| i.wrapping_mul(0x9e37_79b9_7f4a_7c15))
                .collect::<Vec<_>>(),
        );
        let signed: Vec<u64> = [-5i64, 0, i64::MIN, i64::MAX, -1_000_000_000]
            .iter()
            .map(|&v| from_signed(v))
            .collect();
        round_trip(&signed);
        assert_eq!(to_signed(from_signed(-42)), -42);
        assert!(from_signed(-1) < from_signed(0));
    }

    /// A column patched reads back as one packed with the values changed,
    /// in place where they fit their runs and packed again where not; its
    /// runs' bounds still hold every value.
    #[test]
    fn a_patched_column_reads_as_one_packed_so() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for trial in 0..400 {
            let len = 1 + (next() % 700) as usize;
            let spread = [0, 1, 1 << 10, 1 << 31, u64::MAX][trial % 5];
            let base = next() % (1 << 40);
            let mut values: Vec<u64> = (0..len)
                .map(|_| {
                    base.wrapping_add(if spread == 0 {
                        0
                    } else {
                        next() % spread.max(1)
                    })
                })
                .collect();
            let bytes = pack(&values);
            let changes: Vec<(usize, u64)> = (0..=(next() % 4))
                .map(|_| {
                    let i = (next() % len as u64) as usize;
                    // Mostly within the run's reach, sometimes far out.
                    let v = if next() % 3 == 0 {
                        next()
                    } else {
                        values[i].wrapping_add(next() % 16).wrapping_sub(8)
                    };
                    (i, v)
                })
                .collect();
            for &(i, v) in &changes {
                values[i] = v;
            }
            let patched = patch(&bytes, &changes).unwrap();
            let column = Packed::read(&patched).unwrap();
            for (i, &v) in values.iter().enumerate() {
                assert_eq!(column.get(i), v, "trial {trial}, value {i}");
                let (low, high) = column.run_bounds(i);
                assert!(low <= v && v <= high, "trial {trial}, value {i}'s run");
            }
        }
        assert_eq!(patch(&pack(&[1, 2]), &[(2, 0)]), Err(Malformed));
    }

    #[test]
    fn a_run_of_one_value_takes_no_bits() {
        let bytes = pack(&[3; 64]);
        assert_eq!(bytes.len(), 8 + 17 + 8, "no word of bits but the one past");
    }

    #[test]
    fn a_damaged_column_is_an_error_never_a_read_past_its_end() {
        let bytes = pack(&(0..500).collect::<Vec<_>>());
        for cut in [0, 7, 9, bytes.len() / 2, bytes.len() - 8] {
            assert_eq!(
                Packed::read(&bytes[..cut]).err(),
                Some(Malformed),
                "cut at {cut}"
            );
        }
        let mut wide = bytes.clone();
        wide[8 + 8 * 8] = 65; // the first run's width
        assert!(Packed::read(&wide).is_err());
    }
}
