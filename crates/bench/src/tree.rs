//! A synthetic tree to measure on (`docs/PLAN.md`, "Measured on the
//! runners"): so many entries, in folders shaped as a home's are, the same
//! on every system for the same count. Phase 1's measurement had one of
//! 1.04 million entries in 70,481 folders, some 15 entries a folder, and
//! this one is shaped alike.
//!
//! Its names are synthetic, as every test's are (AGENTS.md, "Privacy and
//! fixtures"): words, spaces, accents and other scripts, as a home's names
//! hold them, numbered so that no two in a folder are the same, and never
//! a name some system refuses (no `:`, `*`, `?`, `"`, `<`, `>`, `|` or
//! `\`, nothing ending in a dot or a space, none of Windows' device
//! names). Its files are empty: what is measured is names.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What was made.
pub struct Made {
    pub folders: u64,
    pub files: u64,
    pub deepest: usize,
    pub took: Duration,
}

/// Below this depth a folder holds files alone.
const DEEPEST: usize = 12;

const WORDS: &[&str] = &[
    "notes",
    "report",
    "draft",
    "photo",
    "invoice",
    "budget",
    "summary",
    "meeting",
    "holiday",
    "project",
    "archive",
    "backup",
    "music",
    "video",
    "scan",
    "letter",
    "été",
    "café",
    "résumé",
    "naïve",
    "über",
    "Ελλάδα",
    "日本語",
    "проект",
    "عربي",
    "数据",
];

const EXTENSIONS: &[&str] = &[
    ".txt", ".md", ".pdf", ".jpg", ".png", ".rs", ".json", ".odt", ".mp3", ".csv", "",
];

/// A generator of numbers, the same from the same seed on every system
/// (splitmix64).
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        // The remainder of a 64-bit number: a bias far below what shapes a
        // tree.
        #[allow(clippy::cast_possible_truncation)]
        let r = (self.next() % n as u64) as usize;
        r
    }

    fn word(&mut self) -> &'static str {
        WORDS[self.below(WORDS.len())]
    }
}

/// A name, the `n`th of its folder: one or two words, with a space, a dash
/// or nothing between, then its number, then, for a file, an extension.
fn name(numbers: &mut Numbers, n: usize, file: bool) -> String {
    let mut name = numbers.word().to_owned();
    if numbers.below(3) == 0 {
        name.push_str([" ", "-", ""][numbers.below(3)]);
        name.push_str(numbers.word());
    }
    let _ = write!(name, " {n}");
    if file {
        name.push_str(EXTENSIONS[numbers.below(EXTENSIONS.len())]);
    }
    name
}

/// Make `entries` entries below `dir`, which must hold nothing: one in 15
/// a folder, each folder's parent drawn among those made before it, as a
/// home grows, none deeper than 12; then the files, each folder given 0
/// to twice the mean in turn until the count is met.
pub fn make(dir: &Path, entries: u64) -> Result<Made, String> {
    let begun = Instant::now();
    let mut numbers = Numbers(0x5eed_5eed);
    let fail = |what: &str, e: std::io::Error| format!("cannot make a {what}: {e}");
    if std::fs::read_dir(dir)
        .map_err(|e| fail("tree", e))?
        .next()
        .is_some()
    {
        return Err("the tree's folder holds something already: give an empty one".into());
    }
    let folders = entries / 15;
    // Each folder: its path, its depth, how many entries it holds so far.
    let mut made: Vec<(PathBuf, usize, usize)> = vec![(dir.to_path_buf(), 0, 0)];
    // Those a folder may yet be made in: above the deepest.
    let mut open: Vec<usize> = vec![0];
    for _ in 0..folders {
        let parent = open[numbers.below(open.len())];
        let (at, depth, held) = made[parent].clone();
        made[parent].2 += 1;
        let path = at.join(name(&mut numbers, held, false));
        std::fs::create_dir(&path).map_err(|e| fail("folder", e))?;
        if depth + 1 < DEEPEST {
            open.push(made.len());
        }
        made.push((path, depth + 1, 0));
    }
    let files = entries - folders;
    let room = 2 * usize::try_from(files / made.len() as u64).unwrap_or(usize::MAX) + 1;
    let mut left = files;
    let mut turn = 0;
    while left > 0 {
        let folder = turn % made.len();
        turn += 1;
        let given = (numbers.below(room) as u64).min(left);
        for _ in 0..given {
            let held = made[folder].2;
            made[folder].2 += 1;
            let path = made[folder].0.join(name(&mut numbers, held, true));
            std::fs::File::create(&path).map_err(|e| fail("file", e))?;
        }
        left -= given;
    }
    Ok(Made {
        folders,
        files,
        deepest: made.iter().map(|m| m.1).max().unwrap_or(0),
        took: begun.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The count asked is met, one in 15 a folder, none deeper than 12, the same tree from the
    /// same count; no name holds what a system refuses.
    #[test]
    fn the_tree_is_made_as_asked_and_alike_each_time() {
        let s = skwird_testkit::Scratch::new("bench-tree");
        let a = s.dir("a");
        let made = make(&a, 3_000).unwrap();
        assert_eq!(made.folders + made.files, 3_000);
        assert_eq!(made.folders, 200, "one in 15");
        assert!(made.deepest <= DEEPEST);
        let b = s.dir("b");
        make(&b, 3_000).unwrap();
        let names = |root: &Path| {
            let mut all = Vec::new();
            let mut todo = vec![root.to_path_buf()];
            while let Some(d) = todo.pop() {
                for e in std::fs::read_dir(&d).unwrap() {
                    let e = e.unwrap();
                    let name = e.file_name().into_string().unwrap();
                    assert!(
                        !name.contains([':', '*', '?', '"', '<', '>', '|', '\\'])
                            && !name.ends_with(['.', ' ']),
                        "{name:?}"
                    );
                    all.push(e.path().strip_prefix(root).unwrap().to_path_buf());
                    if e.file_type().unwrap().is_dir() {
                        todo.push(e.path());
                    }
                }
            }
            all.sort();
            all
        };
        assert_eq!(names(&a), names(&b));
        assert!(make(&a, 10).is_err(), "a folder that holds something");
    }
}
