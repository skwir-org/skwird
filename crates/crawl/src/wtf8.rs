//! Names as bytes on Windows, whose names are UTF-16 (`docs/PLAN.md`,
//! phase 4): their WTF-8, UTF-8 for every valid name, an unpaired
//! surrogate kept as the three bytes its code point would take. Nothing
//! is lost either way, and a name that is valid UTF-16 is its UTF-8, as
//! the protocol carries names and the index matches them.
//!
//! The encoding is skwird's own, written out here: not Rust's encoded
//! bytes of an `OsStr`, which it does not promise.

/// Put the UTF-16 units `wide` at the end of `out` as WTF-8.
pub fn encode(wide: impl IntoIterator<Item = u16>, out: &mut Vec<u8>) {
    for unit in char::decode_utf16(wide) {
        let point = match unit {
            Ok(c) => u32::from(c),
            Err(e) => u32::from(e.unpaired_surrogate()),
        };
        push_point(point, out);
    }
}

/// A code point as UTF-8, a surrogate's as any other's of its range.
// Each shift and mask leaves a value below 256.
#[allow(clippy::cast_possible_truncation)]
fn push_point(point: u32, out: &mut Vec<u8>) {
    match point {
        0..0x80 => out.push(point as u8),
        0x80..0x800 => out.extend([0xC0 | (point >> 6) as u8, 0x80 | (point & 0x3F) as u8]),
        0x800..0x1_0000 => out.extend([
            0xE0 | (point >> 12) as u8,
            0x80 | ((point >> 6) & 0x3F) as u8,
            0x80 | (point & 0x3F) as u8,
        ]),
        _ => out.extend([
            0xF0 | (point >> 18) as u8,
            0x80 | ((point >> 12) & 0x3F) as u8,
            0x80 | ((point >> 6) & 0x3F) as u8,
            0x80 | (point & 0x3F) as u8,
        ]),
    }
}

/// The UTF-16 units WTF-8 `bytes` name. A byte [`encode`] never writes
/// there is U+FFFD, as a lossy UTF-8 reading has it.
pub fn decode(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some((point, len)) = point_at(&bytes[i..]) else {
            out.push(0xFFFD);
            i += 1;
            continue;
        };
        if let Some(low) = point.checked_sub(0x1_0000) {
            // Below 0x10_0000: each half fits a unit.
            #[allow(clippy::cast_possible_truncation)]
            out.extend([0xD800 + (low >> 10) as u16, 0xDC00 + (low & 0x3FF) as u16]);
        } else {
            // Below 0x1_0000.
            #[allow(clippy::cast_possible_truncation)]
            out.push(point as u16);
        }
        i += len;
    }
    out
}

/// The code point the bytes start with, and how many it takes: none for
/// a sequence cut short, overlong, or past U+10FFFF.
fn point_at(bytes: &[u8]) -> Option<(u32, usize)> {
    let first = bytes[0];
    let (len, start) = match first {
        0x00..=0x7F => return Some((u32::from(first), 1)),
        0xC2..=0xDF => (2, u32::from(first & 0x1F)),
        0xE0..=0xEF => (3, u32::from(first & 0x0F)),
        0xF0..=0xF4 => (4, u32::from(first & 0x07)),
        _ => return None,
    };
    let rest = bytes.get(1..len)?;
    if !rest.iter().all(|b| b & 0xC0 == 0x80) {
        return None;
    }
    let point = rest
        .iter()
        .fold(start, |p, b| (p << 6) | u32::from(b & 0x3F));
    let least = [0, 0, 0x80, 0x800, 0x1_0000][len];
    (point >= least && point <= 0x10_FFFF).then_some((point, len))
}

/// `path`'s WTF-8, put at the end of `out`.
#[cfg(windows)]
pub fn push(path: &std::path::Path, out: &mut Vec<u8>) {
    out.extend_from_slice(&bytes(path));
}

/// `path`'s WTF-8: a valid path's UTF-8 borrowed as it is, as nearly
/// every one is; one holding an unpaired surrogate encoded.
#[cfg(windows)]
pub fn bytes(path: &std::path::Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::windows::ffi::OsStrExt;
    if let Some(text) = path.to_str() {
        return std::borrow::Cow::Borrowed(text.as_bytes());
    }
    let mut out = Vec::with_capacity(path.as_os_str().len());
    encode(path.as_os_str().encode_wide(), &mut out);
    std::borrow::Cow::Owned(out)
}

/// The path WTF-8 `bytes` name.
#[cfg(windows)]
pub fn path(bytes: &[u8]) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&decode(bytes)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wtf8(wide: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        encode(wide.iter().copied(), &mut out);
        out
    }

    /// A valid name is its UTF-8, and reads back as it was.
    #[test]
    fn a_valid_name_is_its_utf8() {
        for name in ["notes.txt", "été", "日本語", "a 🙂 b", ""] {
            let wide: Vec<u16> = name.encode_utf16().collect();
            assert_eq!(wtf8(&wide), name.as_bytes(), "{name}");
            assert_eq!(decode(name.as_bytes()), wide, "{name}");
        }
    }

    /// An unpaired surrogate, which Windows allows in a name and UTF-8
    /// cannot hold, is kept and read back.
    #[test]
    fn an_unpaired_surrogate_is_kept() {
        for wide in [
            &[0x61, 0xD800, 0x62][..],
            &[0xDC00],
            &[0xD83D, 0x61],
            &[0x61, 0xDBFF],
        ] {
            let bytes = wtf8(wide);
            assert!(std::str::from_utf8(&bytes).is_err(), "{bytes:x?}");
            assert_eq!(decode(&bytes), wide, "{bytes:x?}");
        }
    }

    /// Bytes no encoding gives read as U+FFFD, never past their end.
    #[test]
    fn what_no_encoding_gives_reads_as_a_replacement() {
        assert_eq!(decode(b"a\xFFb"), [0x61, 0xFFFD, 0x62]);
        assert_eq!(decode(b"\xE6\x97"), [0xFFFD, 0xFFFD]);
        assert_eq!(decode(b"\xC0\x80"), [0xFFFD, 0xFFFD], "overlong");
        assert_eq!(decode(b"\xF4\x90\x80\x80"), [0xFFFD; 4], "past U+10FFFF");
    }
}
