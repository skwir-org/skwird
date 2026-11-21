//! Names matched as skwir matches them (`docs/DESIGN.md`, decision 7),
//! so that an answer holds whatever skwir's query plugin would match
//! (its `NamePat`, in `crates/plugins/query/src/lang.rs`):
//!
//! - a substring, after `str::to_lowercase` of both sides, or exactly
//!   when the case is asked to count;
//! - a glob, as `globset` compiles it, case-insensitive unless asked,
//!   with no separator rule;
//! - both on the name's lossy UTF-8 form.
//!
//! Each matcher also says which [`mask`](crate::names::mask) bits a name
//! must hold to match, so most names are passed over unread. That test
//! may let a name through that does not match, never keep out one that
//! does:
//!
//! - a substring's bits come from it lowercased as the names were, and
//!   lowercasing keeps every character's bits but sigma's, whose forms
//!   are all non-ASCII;
//! - a glob folds case as regular expressions do (`ſ` matches `s`), not
//!   as `to_lowercase`, so its bits come from its ASCII letters alone, and
//!   a name with any non-ASCII character is always read.

use globset::{GlobBuilder, GlobMatcher};

use crate::names::{NOT_ASCII, lowered, mask};

/// A name to look for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// As skwir's query plugin takes a term: a glob where it holds `*`,
    /// `?` or `[` and compiles as one, otherwise a substring.
    Skwir(String),
    Substring(String),
    Glob(String),
    /// The whole name.
    Exact(String),
}

/// A pattern, and whether case counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameQuery {
    pub pattern: Pattern,
    pub case_sensitive: bool,
}

enum How {
    Substring {
        needle: String,
        case_sensitive: bool,
    },
    Glob(GlobMatcher),
    Exact {
        text: String,
        case_sensitive: bool,
    },
}

/// A compiled [`NameQuery`].
pub struct Matcher {
    how: How,
    /// Bits a name's mask must hold.
    required: u64,
    /// Whether a name with a character that is not ASCII is always read.
    read_not_ascii: bool,
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

fn glob(text: &str, case_sensitive: bool) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(text)
        .case_insensitive(!case_sensitive)
        .literal_separator(false)
        .build()
        .map(|g| g.compile_matcher())
}

impl Matcher {
    /// The matcher for `query`, or why its glob does not compile.
    pub fn new(query: &NameQuery) -> Result<Matcher, String> {
        let cs = query.case_sensitive;
        let substring = |text: &str| Matcher {
            how: How::Substring {
                needle: if cs {
                    text.to_owned()
                } else {
                    text.to_lowercase()
                },
                case_sensitive: cs,
            },
            required: mask(&text.to_lowercase()),
            read_not_ascii: false,
        };
        let globbed = |g: GlobMatcher, text: &str| Matcher {
            how: How::Glob(g),
            required: mask(&glob_literals(text)),
            read_not_ascii: true,
        };
        Ok(match &query.pattern {
            Pattern::Skwir(text) => match text
                .contains(['*', '?', '['])
                .then(|| glob(text, cs).ok())
                .flatten()
            {
                Some(g) => globbed(g, text),
                // A name like `a[b`, which is no glob, is searched for as
                // written.
                None => substring(text),
            },
            Pattern::Substring(text) => substring(text),
            Pattern::Glob(text) => globbed(glob(text, cs).map_err(|e| e.to_string())?, text),
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

    /// Whether a name with this mask may match: if not, it does not.
    pub fn could(&self, name_mask: u64) -> bool {
        (self.read_not_ascii && name_mask & (1 << NOT_ASCII) != 0)
            || name_mask & self.required == self.required
    }

    /// Whether the name, as the filesystem holds it, matches.
    pub fn matches(&self, name: &[u8]) -> bool {
        match &self.how {
            How::Substring {
                needle,
                case_sensitive: false,
            } => {
                if name.is_ascii() {
                    // An ASCII name lowercases to ASCII alone.
                    needle.is_ascii() && ascii_contains_lowered(name, needle.as_bytes())
                } else {
                    lowered(name).contains(needle.as_str())
                }
            }
            How::Substring {
                needle,
                case_sensitive: true,
            } => String::from_utf8_lossy(name).contains(needle.as_str()),
            How::Glob(g) => g.is_match(&*String::from_utf8_lossy(name)),
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

/// Whether `name`, lowercased as ASCII, holds `needle`, already lowercase.
fn ascii_contains_lowered(name: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    name.windows(needle.len()).any(|w| {
        w.iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What skwir's `NamePat` answers, written as it is, to hold the
    /// matcher to.
    fn skwir_matches(text: &str, case_sensitive: bool, name: &[u8]) -> bool {
        let name = String::from_utf8_lossy(name);
        let glob = text
            .contains(['*', '?', '['])
            .then(|| {
                GlobBuilder::new(text)
                    .case_insensitive(!case_sensitive)
                    .literal_separator(false)
                    .build()
                    .ok()
            })
            .flatten()
            .map(|g| g.compile_matcher());
        match glob {
            Some(g) => g.is_match(&*name),
            None if case_sensitive => name.contains(text),
            None => name.to_lowercase().contains(&text.to_lowercase()),
        }
    }

    const NAMES: &[&[u8]] = &[
        b"Report 2026.PDF",
        b"report-final.pdf",
        b"notes.txt",
        "\u{130}stanbul.jpg".as_bytes(), // İ lowercases to i and a dot above
        "\u{212a}elvin.md".as_bytes(),   // the Kelvin sign lowercases to k
        "\u{17f}ong s".as_bytes(),       // ſ: a glob folds it to s
        "ΟΔΟΣ.txt".as_bytes(),           // a final sigma when lowercased
        "Straße".as_bytes(),
        b"\xffbroken-report",
        b"a[b",
        b"",
        b"UPPER",
    ];

    const TERMS: &[&str] = &[
        "report",
        "REPORT",
        "Report",
        ".pdf",
        "*.pdf",
        "*.PDF",
        "rep*",
        "r?port*",
        "i",
        "İ",
        "k",
        "*k*",
        "s",
        "*s*",
        "*S*",
        "οδος",
        "ς",
        "σ",
        "ss",
        "ß",
        "a[b",
        "[ab]*",
        "*[0-9]*",
        "{notes,upper}*",
        "",
        "\u{fffd}",
        "\\*",
        "*\\[*",
    ];

    /// For every term, both cases, every name: the matcher answers as
    /// skwir does, and its mask never keeps out a name that matches.
    #[test]
    fn names_match_as_skwir_matches_them() {
        for &term in TERMS {
            for cs in [false, true] {
                let matcher = Matcher::new(&NameQuery {
                    pattern: Pattern::Skwir(term.to_owned()),
                    case_sensitive: cs,
                })
                .unwrap();
                for &name in NAMES {
                    let expect = skwir_matches(term, cs, name);
                    let shown = String::from_utf8_lossy(name);
                    assert_eq!(
                        matcher.matches(name),
                        expect,
                        "{term:?} ({cs}) on {shown:?}"
                    );
                    if expect {
                        assert!(
                            matcher.could(mask(&lowered(name))),
                            "the mask keeps out {shown:?} for {term:?} ({cs})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_mask_passes_over_names_that_cannot_match() {
        let m = Matcher::new(&NameQuery {
            pattern: Pattern::Skwir("zebra".into()),
            case_sensitive: false,
        })
        .unwrap();
        assert!(!m.could(mask("report.pdf")));
        assert!(m.could(mask("a zebra")));
        let g = Matcher::new(&NameQuery {
            pattern: Pattern::Glob("*zz*".into()),
            case_sensitive: false,
        })
        .unwrap();
        assert!(!g.could(mask("report.pdf")));
        assert!(g.could(mask("réport")), "not ASCII: always read");
    }

    #[test]
    fn exact_names_match_whole() {
        let m = Matcher::new(&NameQuery {
            pattern: Pattern::Exact("Notes.TXT".into()),
            case_sensitive: false,
        })
        .unwrap();
        assert!(m.matches(b"notes.txt"));
        assert!(!m.matches(b"notes.txt.bak"));
    }
}
