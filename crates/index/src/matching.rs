//! Names matched as skwir matches them (`docs/DESIGN.md`, decision 7),
//! so that an answer holds whatever skwir's matcher would match (its
//! `NameMatch`, in `crates/core/src/name_match.rs`):
//!
//! - **a substring or a glob:** a glob where the term holds `*`, `?` or
//!   `[` and compiles as one (globset, no separator rule), else a
//!   substring, searched for as written (`a[b`);
//! - **case** is ignored unless asked to count: both sides lowercased
//!   with `str::to_lowercase` (`ß` is not `ss`). A glob whose literal
//!   parts are ASCII folds case as globset does, in ASCII alone, on the
//!   name as it is; one whose literal parts hold more, with case ignored,
//!   is matched on the name lowercased, those parts lowercased first;
//! - **both Unicode forms** (open question 5, decided for 0.2): a term is
//!   matched in its composed and its decomposed form (NFC and NFD),
//!   lowercased first where case is ignored, then normalized; a glob in
//!   its literal parts alone, its wildcards, classes and escapes left as
//!   typed. macOS' programs often write names decomposed, and keyboards
//!   type them composed. No name is rewritten, and the index is
//!   unchanged: a name is matched as it is against the two;
//! - **one pattern where both forms are the same:** every ASCII term, and
//!   most others (Cyrillic, Han);
//! - on the name's lossy UTF-8 form.
//!
//! Each matcher also says which [`mask`](crate::names::mask) bits a name
//! must hold to match, so most names are passed over unread. That test
//! may let a name through that does not match, never keep out one that
//! does:
//!
//! - a term's bits are those both its forms hold (a decomposed `é` holds
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
    /// Its two forms, matched in one pass.
    Two(GlobSet),
}

impl Globs {
    fn is_match(&self, text: &str) -> bool {
        match self {
            Globs::One(glob) => glob.is_match(text),
            Globs::Two(set) => set.is_match(text),
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

/// `text` in its composed and decomposed forms (NFC, then NFD), one where
/// they are the same: every ASCII text, given back as it is.
pub fn forms(text: &str) -> Vec<String> {
    if text.is_ascii() {
        return vec![text.to_owned()];
    }
    let composed: String = text.nfc().collect();
    let decomposed: String = text.nfd().collect();
    if composed == decomposed {
        vec![composed]
    } else {
        vec![composed, decomposed]
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
            Cow::Owned(text.to_lowercase())
        };
        let forms = forms(&base);
        Matcher {
            required: shared_mask(forms.iter().map(String::as_str), |f| {
                mask(&f.to_lowercase())
            }),
            how: How::Substring {
                forms,
                case_sensitive,
            },
            read_not_ascii: false,
        }
    }

    /// `text` as a glob in its forms, or why it does not compile as typed.
    fn glob(text: &str, case_sensitive: bool) -> Result<Matcher, globset::Error> {
        let typed = compiled(text, case_sensitive)?;
        let parts = glob_parts(text);
        if parts
            .iter()
            .all(|(literal, part)| !literal || part.is_ascii())
        {
            // Nothing to lowercase or normalize: the glob as typed.
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
        let form = |normalize: fn(&str) -> String| -> String {
            parts
                .iter()
                .map(|&(literal, part)| match (literal, lowered) {
                    (false, _) => Cow::Borrowed(part),
                    (true, false) => Cow::Owned(normalize(part)),
                    (true, true) => Cow::Owned(normalize(&part.to_lowercase())),
                })
                .collect()
        };
        let composed = form(|part| part.nfc().collect());
        let decomposed = form(|part| part.nfd().collect());
        let required = shared_mask([composed.as_str(), decomposed.as_str()], |f| {
            mask(&glob_literals(f))
        });
        let globs = if composed == decomposed {
            Globs::One(compiled(&composed, case_sensitive)?.compile_matcher())
        } else {
            let mut set = GlobSetBuilder::new();
            set.add(compiled(&composed, case_sensitive)?)
                .add(compiled(&decomposed, case_sensitive)?);
            Globs::Two(set.build()?)
        };
        Ok(Matcher {
            how: How::Glob { globs, lowered },
            required,
            read_not_ascii: true,
        })
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
                case_sensitive,
            } => {
                if name.is_ascii() {
                    // An ASCII name lowercases to ASCII alone, and a term
                    // of two forms has no ASCII one: only an ASCII form
                    // can be in it, compared byte by byte, case folded as
                    // it goes where it is ignored.
                    let needle = forms[0].as_bytes();
                    return forms.len() == 1
                        && needle.is_ascii()
                        && ascii_contains(name, needle, *case_sensitive);
                }
                let hay = if *case_sensitive {
                    String::from_utf8_lossy(name)
                } else {
                    Cow::Owned(lowered(name))
                };
                forms.iter().any(|form| hay.contains(form.as_str()))
            }
            How::Glob {
                globs,
                lowered: true,
            } => globs.is_match(&lowered(name)),
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
