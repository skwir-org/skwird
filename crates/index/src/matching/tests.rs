use super::*;

const COMPOSED: &str = "caf\u{e9}.txt";
const DECOMPOSED: &str = "cafe\u{301}.txt";

fn finds(term: &str, case_sensitive: bool, name: &str) -> bool {
    let m = matcher(term, case_sensitive);
    let found = m.matches(name.as_bytes());
    if found {
        assert!(
            m.could(mask(&lowered(name.as_bytes()))),
            "the mask keeps out {name:?} for {term:?} ({case_sensitive})"
        );
    }
    found
}

fn matcher(term: &str, case_sensitive: bool) -> Matcher {
    Matcher::new(&NameQuery {
        pattern: Pattern::Skwir(term.to_owned()),
        case_sensitive,
    })
    .unwrap()
}

/// What skwir's `NameMatch` answers, written plainly from its rules (no
/// mask, no byte-by-byte path), to hold the matcher to.
fn skwir_matches(text: &str, case_sensitive: bool, name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name);
    let typed = text
        .contains(['*', '?', '['])
        .then(|| compiled(text, case_sensitive).ok())
        .flatten();
    let both = |t: &str| -> Vec<String> { vec![t.nfc().collect(), t.nfd().collect()] };
    if let Some(typed) = typed {
        let parts = glob_parts(text);
        if parts
            .iter()
            .all(|(literal, part)| !literal || part.is_ascii())
        {
            return typed.compile_matcher().is_match(&*name);
        }
        let hay = if case_sensitive {
            name.to_string()
        } else {
            name.to_lowercase()
        };
        return [0, 1].into_iter().any(|form| {
            let glob: String = parts
                .iter()
                .map(|&(literal, part)| {
                    if !literal {
                        return part.to_owned();
                    }
                    let part = if case_sensitive {
                        part.to_owned()
                    } else {
                        part.to_lowercase()
                    };
                    both(&part).swap_remove(form)
                })
                .collect();
            compiled(&glob, case_sensitive)
                .unwrap()
                .compile_matcher()
                .is_match(&hay)
        });
    }
    if case_sensitive {
        both(text).iter().any(|t| name.contains(t.as_str()))
    } else {
        let hay = name.to_lowercase();
        both(&text.to_lowercase())
            .iter()
            .any(|t| hay.contains(t.as_str()))
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
    COMPOSED.as_bytes(),
    DECOMPOSED.as_bytes(),
    "CAF\u{c9}.TXT".as_bytes(),
    "CAFE\u{301} 2.txt".as_bytes(),
    "\u{414}\u{41e}\u{41c}.txt".as_bytes(), // Cyrillic, one form
    b"caf\xe9 cafe\xcc\x81.txt",            // not UTF-8, then decomposed
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
    "caf\u{e9}",
    "cafe\u{301}",
    "CAF\u{c9}",
    "CAFE\u{301}",
    "e",
    "caf\u{e9}*",
    "CAF\u{c9}*",
    "*\u{e9}.txt",
    "cafe\u{301}.*",
    "{caf\u{e9},th\u{e9}}*.txt",
    "caf[\u{e9}]*",
    "CAF*.txt",
    "caf\u{e9}[",
    "\u{434}\u{43e}\u{43c}",
    "\u{434}\u{43e}\u{43c}*",
];

/// For every term, both cases, every name: the matcher answers as
/// skwir does, and its mask never keeps out a name that matches.
#[test]
fn names_match_as_skwir_matches_them() {
    for &term in TERMS {
        for cs in [false, true] {
            let matcher = matcher(term, cs);
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

/// A word typed composed finds a name written decomposed, and back;
/// lowercased first where case is ignored, as typed where it counts.
/// skwir's own cases.
#[test]
fn a_word_finds_a_name_in_either_form() {
    for term in ["caf\u{e9}", "cafe\u{301}", "CAF\u{c9}", "CAFE\u{301}"] {
        for name in [COMPOSED, DECOMPOSED, "CAF\u{c9}.TXT", "CAFE\u{301} 2.txt"] {
            assert!(finds(term, false, name), "{term:?} in {name:?}");
        }
    }
    assert!(finds("CAF\u{c9}", true, "CAFE\u{301}.txt"));
    assert!(finds("CAF\u{c9}", true, "le CAF\u{c9}"));
    assert!(!finds("CAF\u{c9}", true, COMPOSED), "its case counts");
    assert!(
        !finds("caf\u{e9}", false, "cafe.txt"),
        "an accent is no letter"
    );
    // A substring never globs.
    let word = Matcher::new(&NameQuery {
        pattern: Pattern::Substring("caf\u{e9}*".into()),
        case_sensitive: false,
    })
    .unwrap();
    assert!(!word.matches(COMPOSED.as_bytes()));
    // Case folds as `to_lowercase` folds it.
    assert!(!finds("strasse", false, "Stra\u{df}e"));
    assert!(finds("STRA\u{df}E", false, "stra\u{df}e"));
}

/// A glob is normalized in its literal parts alone: `caf\u{e9}*` finds
/// the name written decomposed; its classes are left as typed. skwir's
/// own cases.
#[test]
fn a_glob_finds_a_name_in_either_form_its_classes_as_typed() {
    for term in ["caf\u{e9}*", "CAF\u{c9}*", "*\u{e9}.txt", "cafe\u{301}.*"] {
        for name in [COMPOSED, DECOMPOSED] {
            assert!(finds(term, false, name), "{term:?} for {name:?}");
        }
    }
    assert!(finds("CAF\u{c9}*", false, "CAFE\u{301}.TXT"));
    assert!(finds("CAF\u{c9}*", true, "CAFE\u{301}.txt"));
    assert!(!finds("CAF\u{c9}*", true, COMPOSED), "its case counts");
    assert!(finds("{caf\u{e9},th\u{e9}}*.txt", false, DECOMPOSED));
    // A class is left as typed: decomposed, it would hold an `e`.
    assert!(finds("caf[\u{e9}]*", false, COMPOSED));
    assert!(!finds("caf[\u{e9}]*", false, DECOMPOSED));
    // A glob of ASCII folds case in ASCII alone on the name as it is.
    assert!(finds("CAF*.txt", false, DECOMPOSED));
    assert!(!finds("k*", false, "\u{212a}elvin"));
    assert!(finds("k", false, "\u{212a}elvin"), "a word lowercases it");
    // One that does not compile is a word, as typed.
    assert!(finds("caf\u{e9}[", false, "cafe\u{301}[1].txt"));
    // Of one form, a glob of Cyrillic letters with case ignored
    // lowercases the name.
    assert!(finds(
        "\u{434}\u{43e}\u{43c}*",
        false,
        "\u{414}\u{41e}\u{41c}.txt"
    ));
}

/// Where both forms are one (every ASCII term, Cyrillic and Han ones),
/// one form is kept; two where they differ.
#[test]
fn one_form_where_both_are_one() {
    for term in ["report", "\u{434}\u{43e}\u{43c}", "\u{65e5}\u{672c}"] {
        assert_eq!(forms(term), [term], "{term:?}");
    }
    assert_eq!(forms("cafe\u{301}"), ["caf\u{e9}", "cafe\u{301}"]);
    assert_eq!(forms("caf\u{e9}"), ["caf\u{e9}", "cafe\u{301}"]);
}

/// A name that is not UTF-8 is matched as it reads.
#[test]
fn a_name_that_is_not_utf8_is_matched_as_it_reads() {
    let name = b"caf\xe9 cafe\xcc\x81.txt";
    assert!(matcher("caf\u{e9}", false).matches(name));
    assert!(matcher("*\u{fffd}*", false).matches(name));
}

/// The parts of a glob are those globset reads: literal runs apart from
/// wildcards, classes (a `]` first, a POSIX class within), escapes and
/// alternatives. skwir's own cases.
#[test]
fn a_glob_is_parted_as_globset_reads_it() {
    let parts = glob_parts;
    assert_eq!(
        parts("caf\u{e9}*x?"),
        [(true, "caf\u{e9}"), (false, "*"), (true, "x"), (false, "?")]
    );
    assert_eq!(
        parts("a[]\u{e9}]b[!\u{e9}-\u{ea}]c"),
        [
            (true, "a"),
            (false, "[]\u{e9}]"),
            (true, "b"),
            (false, "[!\u{e9}-\u{ea}]"),
            (true, "c")
        ]
    );
    assert_eq!(
        parts("[[:alpha:]\u{e9}]\u{e9}"),
        [(false, "[[:alpha:]\u{e9}]"), (true, "\u{e9}")]
    );
    assert_eq!(
        parts("{\u{e9},b}"),
        [
            (false, "{"),
            (true, "\u{e9}"),
            (false, ","),
            (true, "b"),
            (false, "}")
        ]
    );
    if !std::path::is_separator('\\') {
        assert_eq!(
            parts("\\*\u{e9}\\\u{e9}"),
            [(false, "\\*"), (true, "\u{e9}"), (false, "\\\u{e9}")]
        );
    }
    assert_eq!(parts("[\u{e9}"), [(false, "[\u{e9}")], "not closed");
    // What globset reads as one class, read as one: each of these
    // compiles, and a name holding the class's own characters matches.
    for (glob, name) in [
        ("[]]x", "]x"),
        ("[!]]x", "ax"),
        ("[[:alpha:]]x", "ax"),
        ("[a-]x", "-x"),
        // globset reads `[:nope:]` whole, then takes its `[` alone.
        ("[[:nope:]]x", "[x"),
    ] {
        assert!(finds(glob, true, name), "{glob} for {name}");
    }
}

#[test]
fn a_mask_passes_over_names_that_cannot_match() {
    let m = matcher("zebra", false);
    assert!(!m.could(mask("report.pdf")));
    assert!(m.could(mask("a zebra")));
    let g = Matcher::new(&NameQuery {
        pattern: Pattern::Glob("*zz*".into()),
        case_sensitive: false,
    })
    .unwrap();
    assert!(!g.could(mask("report.pdf")));
    assert!(g.could(mask("réport")), "not ASCII: always read");
    // A term typed decomposed holds an `e` its composed form does not: a
    // name written composed, which has none, is still read.
    let m = matcher("cafe\u{301}", false);
    assert!(m.could(mask(COMPOSED)));
    assert!(!m.could(mask("caf.txt")), "no accent at all");
}

/// The whole name, as it is: a name in the other form is another name.
#[test]
fn exact_names_match_whole() {
    let m = Matcher::new(&NameQuery {
        pattern: Pattern::Exact("Notes.TXT".into()),
        case_sensitive: false,
    })
    .unwrap();
    assert!(m.matches(b"notes.txt"));
    assert!(!m.matches(b"notes.txt.bak"));
    let m = Matcher::new(&NameQuery {
        pattern: Pattern::Exact(COMPOSED.into()),
        case_sensitive: true,
    })
    .unwrap();
    assert!(m.matches(COMPOSED.as_bytes()));
    assert!(!m.matches(DECOMPOSED.as_bytes()));
}
