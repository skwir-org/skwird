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
/// mask, no byte-by-byte path), to hold the matcher to. A glob's classes
/// and escapes are folded by the matcher's own `fold_part`, held to
/// skwir's cases apart (`a_glob_folds_its_escapes_and_classes`).
fn skwir_matches(text: &str, case_sensitive: bool, name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name);
    let typed = text
        .contains(['*', '?', '['])
        .then(|| compiled(text, case_sensitive).ok())
        .flatten();
    // As typed, composed, decomposed.
    let three =
        |t: &str| -> Vec<String> { vec![t.to_owned(), t.nfc().collect(), t.nfd().collect()] };
    // Lowercased, a final sigma a sigma.
    let low = |t: &str| t.to_lowercase().replace('ς', "σ");
    if let Some(typed) = typed {
        if text.is_ascii() {
            return typed.compile_matcher().is_match(&*name);
        }
        let parts = glob_parts(text);
        let hay = if case_sensitive {
            name.to_string()
        } else {
            low(&name)
        };
        return [0, 1, 2].into_iter().any(|form| {
            let glob: String = parts
                .iter()
                .map(|&(literal, part)| {
                    if !literal {
                        return if case_sensitive {
                            part.to_owned()
                        } else {
                            fold_part(false, part).into_owned()
                        };
                    }
                    let part = if case_sensitive {
                        part.to_owned()
                    } else {
                        low(part)
                    };
                    three(&part).swap_remove(form)
                })
                .collect();
            compiled(&glob, case_sensitive)
                .unwrap()
                .compile_matcher()
                .is_match(&hay)
        });
    }
    if case_sensitive {
        three(text).iter().any(|t| name.contains(t.as_str()))
    } else {
        let hay = low(&name);
        three(&low(text)).iter().any(|t| hay.contains(t.as_str()))
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
    // Characters that composing and decomposing both change.
    "\u{f900}.txt".as_bytes(),   // a CJK compatibility ideograph
    "\u{958}.txt".as_bytes(),    // Devanagari qa, excluded from composing
    "\u{1f71}.txt".as_bytes(),   // alpha with oxia
    "\u{212b} 1.txt".as_bytes(), // the Ångström sign
    "\u{2126} 2.txt".as_bytes(), // the Ohm sign
    "\u{fb2a}.txt".as_bytes(),   // a Hebrew presentation form
    "ΟΔΟΣ".as_bytes(),           // its sigma final when lowercased
    b"OK.txt",
    "ΛΟΓΑΡΙΑΣΜΟΣ.pdf".as_bytes(),
    "été.txt".as_bytes(),
    "ÉTÉ.txt".as_bytes(),
    "ς.txt".as_bytes(),
    "ωmega.txt".as_bytes(),
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
    "\u{f900}",
    "\u{958}",
    "\u{1f71}",
    "\u{212b}",
    "\u{2126}",
    "\u{fb2a}",
    "*\u{f900}*",
    "\u{2126} *",
    "ΟΔΟΣ",
    "*ΟΔΟΣ*",
    "οδοσ",
    "ΟΔΟΣ.txt",
    "\u{212a}",
    "λογαριασμος",
    "ΟΔΟΣ*",
    "\\É*",
    "é*\\É*",
    "[Ω]*",
    "[!ÉΩ]*",
    "[À-ÞÉ]*",
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
    assert_eq!(forms("cafe\u{301}"), ["cafe\u{301}", "caf\u{e9}"]);
    assert_eq!(forms("caf\u{e9}"), ["caf\u{e9}", "cafe\u{301}"]);
    // Both forms change it: as typed, then the two.
    assert_eq!(forms("\u{1f71}"), ["\u{1f71}", "\u{3ac}", "\u{3b1}\u{301}"]);
    assert_eq!(forms("\u{2126}"), ["\u{2126}", "\u{3a9}"]);
}

/// A term holding a character that composing and decomposing both change
/// finds a name holding the very same character: it is matched as typed
/// too. Each of those characters, with case ignored and with it counted.
#[test]
fn a_character_both_forms_change_finds_itself() {
    for (term, name) in [
        ("\u{f900}", "\u{f900}.txt"),
        ("\u{958}", "\u{958}.txt"),
        ("\u{1f71}", "\u{1f71}.txt"),
        ("\u{212b}", "\u{212b} 1.txt"),
        ("\u{2126}", "\u{2126} 2.txt"),
        ("\u{fb2a}", "\u{fb2a}.txt"),
        ("*\u{f900}*", "\u{f900}.txt"),
        ("\u{2126} *", "\u{2126} 2.txt"),
    ] {
        for cs in [false, true] {
            assert!(finds(term, cs, name), "{term:?} ({cs}) in {name:?}");
        }
    }
    // And still the other forms: the Ohm sign typed finds omega.
    assert!(finds("\u{2126}", true, "\u{3a9}.txt"));
}

/// Where case is ignored, a final sigma is a sigma: `ΟΔΟΣ` typed, which
/// lowers alone to a final `ς`, finds `ΟΔΟΣ.txt`, whose `Σ` lowers to `σ`
/// there, and back; a glob alike. Where case counts, the letters are as
/// typed.
#[test]
fn a_final_sigma_is_a_sigma_where_case_is_ignored() {
    for term in ["ΟΔΟΣ", "οδος", "οδοσ", "*ΟΔΟΣ*", "οδος*"] {
        for name in ["ΟΔΟΣ.txt", "ΟΔΟΣ", "οδος.txt"] {
            assert!(finds(term, false, name), "{term:?} in {name:?}");
        }
    }
    assert!(finds("ΟΔΟΣ", true, "ΟΔΟΣ.txt"));
    assert!(!finds("οδος", true, "οδοσ.txt"), "case counts: ς is not σ");
}

/// skwir's own cases (skwir-20, 2026-11-25, each checked through skwir's
/// matcher), as (term, name, case counts, found), each matched as the
/// index matches it: by the matcher, its mask letting it through.
fn hold_to(cases: &[(&str, &str, bool, bool)]) {
    for &(term, name, cs, expect) in cases {
        assert_eq!(finds(term, cs, name), expect, "{term:?} ({cs}) in {name:?}");
        assert_eq!(
            skwir_matches(term, cs, name.as_bytes()),
            expect,
            "the reference: {term:?} ({cs}) in {name:?}"
        );
    }
}

/// NM-1: a term finds its own text where neither NFC nor NFD keeps it;
/// an ASCII name is compared with the term's one ASCII form (the Kelvin
/// sign's `K`, case counting).
#[test]
fn skwirs_cases_of_the_three_forms() {
    hold_to(&[
        ("\u{f900}", "\u{f900}.txt", false, true),
        ("\u{f900}", "\u{f900}.txt", true, true),
        ("\u{958}", "\u{958}.txt", false, true),
        ("\u{1f71}", "\u{1f71}.txt", false, true),
        ("\u{212b}", "\u{212b}.txt", true, true),
        ("\u{2126}", "\u{2126}.txt", true, true),
        ("\u{fb2a}", "\u{fb2a}.txt", false, true),
        ("\u{212a}", "\u{212a}.txt", true, true),
        ("\u{f900}*", "\u{f900}.txt", false, true),
        ("\u{f900}*", "\u{f900}.txt", true, true),
        ("*\u{958}.txt", "\u{958}.txt", false, true),
        // Unchanged.
        ("\u{f900}", "\u{8c48}.txt", false, true),
        ("\u{212b}", "\u{c5}.txt", true, true),
        ("\u{212b}", "A\u{30a}.txt", true, true),
        ("\u{212a}", "OK.txt", true, true),
        ("\u{212a}", "OK \u{e9}.txt", true, true),
    ]);
    assert_eq!(forms("\u{1f71}"), ["\u{1f71}", "\u{3ac}", "\u{3b1}\u{301}"]);
    assert_eq!(forms("cafe\u{301}"), ["cafe\u{301}", "caf\u{e9}"]);
    for (term, count) in [
        ("\u{f900}", 2),
        ("\u{1f71}", 3),
        ("\u{1f71}*", 3),
        ("caf\u{e9}", 2),
        ("cafe\u{301}", 2),
    ] {
        assert_eq!(matcher(term, false).patterns(), count, "{term:?}");
    }
}

/// NM-2: where case is ignored, `ς` is taken as `σ`, on the term and the
/// name alike.
#[test]
fn skwirs_cases_of_the_final_sigma() {
    hold_to(&[
        ("ΟΔΟΣ", "ΟΔΟΣ.txt", false, true),
        ("οδος", "ΟΔΟΣ.txt", false, true),
        ("λογαριασμος", "ΛΟΓΑΡΙΑΣΜΟΣ.pdf", false, true),
        ("οδοσ", "οδος.txt", false, true),
        ("ΟΔΟΣ*", "ΟΔΟΣ.txt", false, true),
        ("*οδος.txt", "ΟΔΟΣ.txt", false, true),
        // Unchanged.
        ("ΟΔΟΣ", "οδος.txt", false, true),
        ("οδος", "ΟΔΟΣ ΜΟΥ.txt", false, true),
        ("οδος", "οδοσ.txt", true, false),
    ]);
    assert_eq!(fold("ΟΔΟΣ"), "οδοσ");
    assert_eq!(fold("ΣΑΣ"), "σασ");
    assert_eq!(fold("Straße"), "straße");
}

/// NM-4: a glob with case ignored folds what its escapes escape, and its
/// classes' single members, a range's ends left as typed. A backslash
/// escapes in a glob where it separates no path: on Windows it is a
/// literal, so the cases with an escape are not Windows' (skwir's
/// `glob_parts` and globset read it so there too).
#[test]
fn a_glob_folds_its_escapes_and_classes() {
    #[cfg(not(windows))]
    hold_to(&[
        ("é*\\É*", "étÉ.txt", false, true),
        ("é*\\É*", "ÉTÉ.txt", false, true),
        ("\\É*", "été.txt", false, true),
        ("\\Σ*", "ς.txt", false, true),
        // Unchanged.
        ("\\É*", "été.txt", true, false),
        ("\\É*", "Été.txt", true, true),
    ]);
    hold_to(&[
        ("[Ω]*", "ωmega.txt", false, true),
        // Unchanged.
        ("[Ω]*", "Ωmega.txt", false, true),
    ]);
    for (class, folded) in [
        ("[É]", "[é]"),
        ("[!ÉΩ]", "[!éω]"),
        ("[^Σ]", "[^σ]"),
        ("[]É]", "[]é]"),
        ("[É-]", "[é-]"),
        ("[-É]", "[-é]"),
        ("[a-cÉ]", "[a-cé]"),
        ("[[:alpha:]É]", "[[:alpha:]é]"),
        ("[À-ÞÉ]", "[À-Þé]"),
        // Unchanged: a range's ends, what folds to two, what is folded.
        ("[É-Ω]", "[É-Ω]"),
        ("[a-É]", "[a-É]"),
        ("[a-c-É]", "[a-c-É]"),
        ("[İ]", "[İ]"),
        ("[é]", "[é]"),
    ] {
        assert_eq!(fold_class(class), folded, "{class:?}");
    }
    let glob = |text: &str| {
        Matcher::new(&NameQuery {
            pattern: Pattern::Glob(text.to_owned()),
            case_sensitive: false,
        })
    };
    assert!(
        glob("[Ω-ΰ]*").is_ok(),
        "a range's ends, unfolded, do not cross"
    );
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
