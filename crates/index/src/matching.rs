//! Names matched as skwir matches them (`docs/DESIGN.md`, decision 7),
//! so that an answer holds whatever skwir's matcher would match (its
//! `NameMatch`, in `crates/core/src/name_match.rs`):
//!
//! - **a substring or a glob:** a glob where the term holds `*`, `?` or
//!   `[` and compiles as one (globset, no separator rule), else a
//!   substring, searched for as written (`a[b`);
//! - **case** is ignored unless asked to count: both sides lowercased
//!   with `str::to_lowercase` (`ß` is not `ss`), then a final sigma (`ς`)
//!   taken as `σ`: lowercased alone, `ΟΔΟΣ` ends in `ς`, and in the name
//!   `ΟΔΟΣ.txt` in `σ`, so the word typed would miss its own file
//!   ([`fold`]). A glob of ASCII folds case as globset does, in ASCII
//!   alone, on the name as it is; one holding more, with case ignored, is
//!   matched on the name folded, itself folded first: its literal parts,
//!   what its escapes escape, its classes' single members, a range's ends
//!   left as typed (globset reads a class beyond ASCII as bytes, and folded
//!   ends may cross);
//! - **its Unicode forms** (open question 5, decided for 0.2): a term is
//!   matched as typed (lowercased where case is ignored), composed and
//!   decomposed (NFC and NFD), lowercased first, then normalized; a glob
//!   in its literal parts alone, its wildcards, classes and escapes left
//!   as typed. macOS' programs often write names decomposed, and
//!   keyboards type them composed; and some characters both forms change
//!   (a CJK compatibility ideograph, the Ohm sign) are found as typed
//!   alone. No name is rewritten, and the index is unchanged: a name is
//!   matched as it is against the three;
//! - **one pattern where the three are the same:** every ASCII term, and
//!   most others (Cyrillic, Han). An ASCII name is compared byte by byte
//!   with the term's one ASCII form, if it has one (the Kelvin sign's `K`,
//!   composed), never lowercased into a string of its own;
//! - on the name's lossy UTF-8 form.
//!
//! Each matcher also says which [`mask`](crate::names::mask) bits a name
//! must hold to match, so most names are passed over unread. That test
//! may let a name through that does not match, never keep out one that
//! does:
//!
//! - a term's bits are those all its forms hold (a decomposed `é` holds
//!   an `e`, a composed one does not);
//! - a substring's bits come from it lowercased as the names were, and
//!   lowercasing keeps every character's bits but sigma's, whose forms
//!   are all non-ASCII;
//! - a glob folds case as regular expressions do (`ſ` matches `s`), not
//!   as `to_lowercase`, so its bits come from its ASCII letters alone, and
//!   a name with any non-ASCII character is always read.

use std::borrow::Cow;

use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use unicode_normalization::UnicodeNormalization;

use crate::names::{NOT_ASCII, lowered, mask};

/// A name to look for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// As skwir takes a term: a glob where it holds `*`, `?` or `[` and
    /// compiles as one, otherwise a substring.
    Skwir(String),
    Substring(String),
    Glob(String),
    /// The whole name, as it is: never in another form, since two names
    /// that differ in form alone are two names.
    Exact(String),
}

/// A pattern, and whether case counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameQuery {
    pub pattern: Pattern,
    pub case_sensitive: bool,
}

enum How {
    /// Found anywhere in the name: the term in its forms, lowercased
    /// unless case counts.
    Substring {
        forms: Vec<String>,
        /// Its one ASCII form, if any: what an ASCII name is compared
        /// with. Never two: an ASCII text is its own NFC and NFD, and a
        /// text's NFD is its NFC's.
        ascii: Option<String>,
        case_sensitive: bool,
    },
    /// The glob in its forms, matched on the name lowercased where
    /// `lowered`, else on the name as it is.
    Glob {
        globs: Globs,
        lowered: bool,
    },
    Exact {
        text: String,
        case_sensitive: bool,
    },
}

enum Globs {
    One(GlobMatcher),
    /// Its forms, two or three, matched in one pass.
    Many(GlobSet),
}

impl Globs {
    fn is_match(&self, text: &str) -> bool {
        match self {
            Globs::One(glob) => glob.is_match(text),
            Globs::Many(set) => set.is_match(text),
        }
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        match self {
            Globs::One(_) => 1,
            Globs::Many(set) => set.len(),
        }
    }
}

/// A compiled [`NameQuery`].
pub struct Matcher {
    how: How,
    /// Bits a name's mask must hold.
    required: u64,
    /// Whether a name with a character that is not ASCII is always read.
    read_not_ascii: bool,
}

/// `text` in its forms: as it is, composed and decomposed (NFC, then
/// NFD), each once: one where the three are the same, every ASCII text.
pub fn forms(text: &str) -> Vec<String> {
    let mut out = vec![text.to_owned()];
    if text.is_ascii() {
        return out;
    }
    for form in [text.nfc().collect::<String>(), text.nfd().collect()] {
        if !out.contains(&form) {
            out.push(form);
        }
    }
    out
}

/// What case ignored makes of a text, a term's and a name's alike:
/// lowercased as `str::to_lowercase` lowercases it (`ß` is not `ss`), and
/// a final sigma `ς` taken as `σ`. `to_lowercase` writes `Σ` as `ς` only
/// at a word's end, which a word typed alone (`ΟΔΟΣ`) has where the same
/// word in a name goes on to its extension (`ΟΔΟΣ.txt`). The same as
/// lowercasing each character alone, then taking `ς` as `σ`. skwir's.
pub fn fold(text: &str) -> String {
    folded(text.to_lowercase())
}

/// [`fold`], of a text already lowercased.
fn folded(lowered: String) -> String {
    if lowered.contains('ς') {
        lowered.replace('ς', "σ")
    } else {
        lowered
    }
}

/// The bits every one of `texts` holds.
fn shared_mask<'a>(texts: impl IntoIterator<Item = &'a str>, of: impl Fn(&str) -> u64) -> u64 {
    texts.into_iter().map(of).fold(u64::MAX, |m, t| m & t)
}

/// The ASCII characters that a glob requires of a name, lowercased: those
/// outside wildcards, up to its first class or alternation, past which
/// nothing is taken for required.
fn glob_literals(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '*' | '?' => {}
            '[' | '{' => break,
            c => out.push(c),
        }
    }
    out.chars()
        .filter(char::is_ascii)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn compiled(text: &str, case_sensitive: bool) -> Result<globset::Glob, globset::Error> {
    GlobBuilder::new(text)
        .case_insensitive(!case_sensitive)
        .literal_separator(false)
        .build()
}

impl Matcher {
    /// The matcher for `query`, or why its glob does not compile.
    pub fn new(query: &NameQuery) -> Result<Matcher, String> {
        let cs = query.case_sensitive;
        Ok(match &query.pattern {
            Pattern::Skwir(text) => match text
                .contains(['*', '?', '['])
                .then(|| Self::glob(text, cs).ok())
                .flatten()
            {
                Some(g) => g,
                // A name like `a[b`, which is no glob, is searched for as
                // written.
                None => Self::substring(text, cs),
            },
            Pattern::Substring(text) => Self::substring(text, cs),
            Pattern::Glob(text) => Self::glob(text, cs).map_err(|e| e.to_string())?,
            Pattern::Exact(text) => Matcher {
                how: How::Exact {
                    text: if cs {
                        text.clone()
                    } else {
                        text.to_lowercase()
                    },
                    case_sensitive: cs,
                },
                required: mask(&text.to_lowercase()),
                read_not_ascii: false,
            },
        })
    }

    /// `text` anywhere in a name, in its forms.
    fn substring(text: &str, case_sensitive: bool) -> Matcher {
        let base = if case_sensitive {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(fold(text))
        };
        let forms = forms(&base);
        Matcher {
            required: shared_mask(forms.iter().map(String::as_str), |f| {
                mask(&f.to_lowercase())
            }),
            how: How::Substring {
                ascii: forms.iter().find(|f| f.is_ascii()).cloned(),
                forms,
                case_sensitive,
            },
            read_not_ascii: false,
        }
    }

    /// `text` as a glob in its forms, or why it does not compile as typed.
    fn glob(text: &str, case_sensitive: bool) -> Result<Matcher, globset::Error> {
        let typed = compiled(text, case_sensitive)?;
        if text.is_ascii() {
            // Nothing to fold or normalize: the glob as typed.
            return Ok(Matcher {
                how: How::Glob {
                    globs: Globs::One(typed.compile_matcher()),
                    lowered: false,
                },
                required: mask(&glob_literals(text)),
                read_not_ascii: true,
            });
        }
        let lowered = !case_sensitive;
        // Each part as it is matched: folded where case is ignored, a
        // literal one then normalized in each form.
        let parts: Vec<(bool, Cow<'_, str>)> = glob_parts(text)
            .into_iter()
            .map(|(literal, part)| {
                let part = if lowered {
                    fold_part(literal, part)
                } else {
                    Cow::Borrowed(part)
                };
                (literal, part)
            })
            .collect();
        let form = |normalize: fn(&str) -> String| -> String {
            parts
                .iter()
                .map(|(literal, part)| {
                    if *literal {
                        Cow::Owned(normalize(part))
                    } else {
                        Cow::Borrowed(&**part)
                    }
                })
                .collect()
        };
        let mut globs: Vec<String> = Vec::with_capacity(3);
        for made in [
            form(str::to_owned),
            form(|part| part.nfc().collect()),
            form(|part| part.nfd().collect()),
        ] {
            if !globs.contains(&made) {
                globs.push(made);
            }
        }
        let required = shared_mask(globs.iter().map(String::as_str), |f| {
            mask(&glob_literals(f))
        });
        let globs = if let [one] = globs.as_slice() {
            Globs::One(compiled(one, case_sensitive)?.compile_matcher())
        } else {
            let mut set = GlobSetBuilder::new();
            for glob in &globs {
                set.add(compiled(glob, case_sensitive)?);
            }
            Globs::Many(set.build()?)
        };
        Ok(Matcher {
            how: How::Glob { globs, lowered },
            required,
            read_not_ascii: true,
        })
    }

    /// How many patterns it matches a name against: more than one only
    /// where the term's forms differ.
    #[cfg(test)]
    fn patterns(&self) -> usize {
        match &self.how {
            How::Substring { forms, .. } => forms.len(),
            How::Glob { globs, .. } => globs.count(),
            How::Exact { .. } => 1,
        }
    }

    /// Whether a name with this mask may match: if not, it does not.
    pub fn could(&self, name_mask: u64) -> bool {
        (self.read_not_ascii && name_mask & (1 << NOT_ASCII) != 0)
            || name_mask & self.required == self.required
    }

    /// Whether the name, as the filesystem holds it, matches.
    pub fn matches(&self, name: &[u8]) -> bool {
        match &self.how {
            How::Substring {
                forms,
                ascii,
                case_sensitive,
            } => {
                if name.is_ascii() {
                    // An ASCII name folds to ASCII alone: only an ASCII
                    // form can be in it (most terms' one form; the Kelvin
                    // sign's, `K`, composed), compared byte by byte, case
                    // folded as it goes where it is ignored.
                    return ascii.as_ref().is_some_and(|needle| {
                        ascii_contains(name, needle.as_bytes(), *case_sensitive)
                    });
                }
                let hay = if *case_sensitive {
                    String::from_utf8_lossy(name)
                } else {
                    Cow::Owned(folded(lowered(name)))
                };
                forms.iter().any(|form| hay.contains(form.as_str()))
            }
            // An ASCII name as it is: lowercasing it changes the case of its
            // ASCII letters alone, which the glob ignores (globset's
            // `(?-u)(?i)`), and it holds no ς to fold.
            How::Glob {
                globs,
                lowered: true,
            } => match std::str::from_utf8(name) {
                Ok(text) if text.is_ascii() => globs.is_match(text),
                _ => globs.is_match(&folded(lowered(name))),
            },
            How::Glob {
                globs,
                lowered: false,
            } => globs.is_match(&String::from_utf8_lossy(name)),
            How::Exact {
                text,
                case_sensitive: true,
            } => String::from_utf8_lossy(name) == text.as_str(),
            How::Exact {
                text,
                case_sensitive: false,
            } => lowered(name) == *text,
        }
    }
}

/// Whether ASCII `name` holds ASCII `needle`, already lowercase where
/// case is ignored, `name` lowercased as it goes.
fn ascii_contains(name: &[u8], needle: &[u8], case_sensitive: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    name.windows(needle.len()).any(|w| {
        if case_sensitive {
            w == needle
        } else {
            w.iter()
                .zip(needle)
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
        }
    })
}

/// A part of a glob ([`glob_parts`]) as case ignored folds it: a literal
/// run whole ([`fold`]), what an escape escapes (each character it folds
/// to escaped again), a class's single members ([`fold_class`]); what is
/// ASCII as it is, which globset folds. skwir's.
fn fold_part(literal: bool, part: &str) -> Cow<'_, str> {
    if part.is_ascii() {
        Cow::Borrowed(part)
    } else if literal {
        Cow::Owned(fold(part))
    } else if let Some(escaped) = part.strip_prefix('\\') {
        Cow::Owned(fold(escaped).chars().flat_map(|c| ['\\', c]).collect())
    } else if part.starts_with('[') {
        fold_class(part)
    } else {
        Cow::Borrowed(part)
    }
}

/// `class`, a class as [`glob_parts`] parts it (its brackets, closed), with
/// each single member beyond ASCII folded where it folds to one character
/// (a class matches one: `İ`, which folds to two, stays). A range's ends
/// stay as typed: globset reads a range beyond ASCII as one of bytes,
/// which folding its ends would not mend, and folded they may cross (`Ω`
/// folds past `ΰ`), which globset refuses. Read as globset reads a class
/// ([`class_end`]). skwir's.
fn fold_class(class: &str) -> Cow<'_, str> {
    let mut at = 1;
    if class[at..].starts_with(['!', '^']) {
        at += 1;
    }
    let body = at;
    // Where each range's ends are, by their first byte.
    let mut ends = vec![false; class.len()];
    // The last member, where a range may yet extend it: none after a
    // POSIX class, whose ends are ASCII.
    let mut last: Option<usize> = None;
    let (mut first, mut in_range) = (true, false);
    let mut close = class.len();
    while let Some(c) = class[at..].chars().next() {
        let here = at;
        at += c.len_utf8();
        match c {
            ']' if !first => {
                close = here;
                break;
            }
            '-' if !first && !in_range => in_range = true,
            '[' if !in_range && posix_class(&class[at..]).is_some() => {
                at += posix_class(&class[at..]).unwrap_or(0);
                last = None;
            }
            _ if in_range => {
                if let Some(start) = last {
                    ends[start] = true;
                }
                ends[here] = true;
                in_range = false;
            }
            _ => last = Some(here),
        }
        first = false;
    }
    let mut folded = String::with_capacity(class.len());
    folded.push_str(&class[..body]);
    let mut changed = false;
    for (offset, c) in class[body..close].char_indices() {
        if !c.is_ascii() && !ends[body + offset] {
            // As `fold` folds it, a character alone.
            let mut lower = c
                .to_lowercase()
                .map(|lower| if lower == 'ς' { 'σ' } else { lower });
            if let (Some(one), None) = (lower.next(), lower.next())
                && one != c
            {
                folded.push(one);
                changed = true;
                continue;
            }
        }
        folded.push(c);
    }
    folded.push_str(&class[close..]);
    if changed {
        Cow::Owned(folded)
    } else {
        Cow::Borrowed(class)
    }
}

/// `glob` in its parts, as globset reads it: runs of literal characters
/// (`true`), and what is no literal (`false`): a wildcard, a class with
/// its brackets, an escape with what it escapes, the braces and commas of
/// alternatives. Every one of those is ASCII, and none composes with a
/// mark after it, so normalizing the runs apart is normalizing the
/// literal text. skwir's, as it parts a glob.
fn glob_parts(glob: &str) -> Vec<(bool, &str)> {
    // globset's default: a backslash escapes, unless it separates paths.
    let escapes = !std::path::is_separator('\\');
    let mut parts = Vec::new();
    let mut literal_from: Option<usize> = None;
    let mut at = 0;
    while let Some(c) = glob[at..].chars().next() {
        let end = match c {
            '*' | '?' | '{' | '}' | ',' => Some(at + 1),
            '\\' if escapes => Some(
                glob[at + 1..]
                    .chars()
                    .next()
                    .map_or(glob.len(), |escaped| at + 1 + escaped.len_utf8()),
            ),
            '[' => Some(class_end(glob, at)),
            _ => None,
        };
        if let Some(end) = end {
            if let Some(from) = literal_from.take() {
                parts.push((true, &glob[from..at]));
            }
            parts.push((false, &glob[at..end]));
            at = end;
        } else {
            literal_from.get_or_insert(at);
            at += c.len_utf8();
        }
    }
    if let Some(from) = literal_from {
        parts.push((true, &glob[from..]));
    }
    parts
}

/// Where the class `glob` opens at `open` (its `[`) ends, past its `]`, as
/// globset parses one: a `!` or `^` first negates it, a `]` first is one of
/// its characters, a `[:name:]` (eight small letters at most) is read
/// whole; the end of the text where it is not closed, which globset
/// refuses.
fn class_end(glob: &str, open: usize) -> usize {
    let mut at = open + 1;
    if glob[at..].starts_with(['!', '^']) {
        at += 1;
    }
    let (mut first, mut in_range) = (true, false);
    while let Some(c) = glob[at..].chars().next() {
        at += c.len_utf8();
        match c {
            ']' if !first => return at,
            ']' => {}
            // A range's dash, or its end where one is open.
            '-' => {
                if !first {
                    in_range = !in_range;
                }
            }
            '[' if !in_range && glob[at..].starts_with(':') => {
                if let Some(len) = posix_class(&glob[at..]) {
                    at += len;
                }
            }
            _ => in_range = false,
        }
        first = false;
    }
    glob.len()
}

/// The length of `rest`, read after a `[` in a class, that globset reads
/// as a POSIX class: `:`, up to eight small ASCII letters, `:]`; whatever
/// the name, as globset consumes it.
fn posix_class(rest: &str) -> Option<usize> {
    let name = rest.strip_prefix(':')?;
    let letters = name
        .bytes()
        .take_while(u8::is_ascii_lowercase)
        .count()
        .min(8);
    name[letters..].starts_with(":]").then_some(1 + letters + 2)
}

#[cfg(test)]
mod tests;
