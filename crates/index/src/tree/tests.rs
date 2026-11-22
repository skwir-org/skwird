//! The format's spike (`docs/PLAN.md`, phase 1, step 2): a subtree stays
//! one range while chunks are rewritten. Random edits of a model tree,
//! each made in the index too, chunks kept small so that every edit
//! cuts or merges some; after each, every folder's range holds exactly
//! its subtree, in pre-order, and every block is the model's.

use std::collections::{BTreeMap, BTreeSet};

use skwird_crawl::Kind;

use super::*;
use crate::block::{Item, State};

/// splitmix64: random enough, and the same run for the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }
}

/// The tree as it should be: each folder's parent and files.
#[derive(Default)]
struct Model {
    parent: BTreeMap<FolderId, Option<FolderId>>,
    files: BTreeMap<FolderId, BTreeSet<u32>>,
    next_file: u32,
}

/// A folder's entry name, apart from every file's.
const FOLDER_NAMES: u32 = 1 << 30;

impl Model {
    fn folders(&self) -> Vec<FolderId> {
        self.parent.keys().copied().collect()
    }

    fn children(&self, folder: FolderId) -> Vec<FolderId> {
        self.parent
            .iter()
            .filter(|(_, p)| **p == Some(folder))
            .map(|(c, _)| *c)
            .collect()
    }

    fn depth(&self, folder: FolderId) -> u32 {
        self.parent[&folder].map_or(0, |p| self.depth(p) + 1)
    }

    /// The folder and every folder below it.
    fn subtree(&self, folder: FolderId) -> BTreeSet<FolderId> {
        let mut out = BTreeSet::from([folder]);
        for child in self.children(folder) {
            out.extend(self.subtree(child));
        }
        out
    }

    /// Its subtree in pre-order, as the tree is built from it.
    fn preorder(&self, folder: FolderId, out: &mut Vec<FolderId>) {
        out.push(folder);
        for child in self.children(folder) {
            self.preorder(child, out);
        }
    }

    fn block(&self, folder: FolderId) -> Block {
        let mut items: Vec<Item> = self.files[&folder]
            .iter()
            .map(|&name| Item {
                name,
                kind: Kind::File,
                ino: u64::from(name),
                attrs: None,
                child: None,
            })
            .chain(self.children(folder).into_iter().map(|child| Item {
                name: FOLDER_NAMES + child,
                kind: Kind::Folder,
                ino: u64::from(child),
                attrs: None,
                child: Some(child),
            }))
            .collect();
        items.sort_by_key(|i| i.name);
        Block {
            folder,
            parent: self.parent[&folder],
            name: self.parent[&folder].map_or(0, |_| FOLDER_NAMES + folder),
            depth: self.depth(folder),
            facts: None,
            state: State::Listed { again: false },
            items,
        }
    }

    fn add_files(&mut self, folder: FolderId, n: usize) {
        for _ in 0..n {
            self.next_file += 1;
            self.files.entry(folder).or_default().insert(self.next_file);
        }
    }

    fn add_folder(&mut self, folder: FolderId, parent: FolderId) {
        self.parent.insert(folder, Some(parent));
        self.files.insert(folder, BTreeSet::new());
    }
}

/// Every folder's range against the model, and every block.
fn assert_matches(tree: &Tree, model: &Model) {
    tree.assert_sound();
    assert_eq!(tree.len(), model.parent.len());
    for folder in model.folders() {
        let mut seen = Vec::new();
        for (chunk, range) in tree.ranges(folder) {
            for b in range {
                let block = tree.chunk(chunk).block(b);
                assert_eq!(
                    block,
                    model.block(block.folder),
                    "block of {}",
                    block.folder
                );
                seen.push(block.folder);
            }
        }
        assert_eq!(
            seen.first(),
            Some(&folder),
            "a subtree starts at its folder"
        );
        let set: BTreeSet<_> = seen.iter().copied().collect();
        assert_eq!(set.len(), seen.len(), "no folder twice");
        assert_eq!(set, model.subtree(folder), "the subtree of {folder}");
    }
}

/// A folder's block of new files put in the tree as the index puts one:
/// patched, else spliced, else its chunk cut again.
fn rewrite(tree: &mut Tree, block: Block) {
    let old = tree.block(block.folder).unwrap();
    if !tree.patch(&old, &block) && !tree.splice(&old, &block) {
        tree.replace(block);
    }
}

fn run(seed: u64, steps: usize, limits: Limits) {
    let mut rng = Rng(seed);
    let mut model = Model::default();
    model.parent.insert(0, None);
    model.files.insert(0, BTreeSet::new());
    model.add_files(0, 5);
    let mut built = Vec::new();
    for id in 1..30 {
        let parent = rng.below(id as usize) as FolderId;
        model.add_folder(id, parent);
        model.add_files(id, rng.below(12));
    }
    model.preorder(0, &mut built);
    let mut tree = Tree::build(built.iter().map(|&f| model.block(f)).collect(), limits);
    assert_matches(&tree, &model);
    for step in 0..steps {
        let folders = model.folders();
        let folder = rng.pick(&folders);
        let shape = tree.shape();
        let edit = rng.below(10);
        // Whether the edit makes, moves or lets go of a folder.
        let mut reshaped = false;
        match edit {
            // A file made, or several.
            0..=2 => {
                let many = rng.below(10) == 0;
                let n = 1 + rng.below(if many { 80 } else { 3 });
                model.add_files(folder, n);
                rewrite(&mut tree, model.block(folder));
            }
            // A file removed.
            3 => {
                if let Some(&file) = model.files[&folder].iter().next() {
                    model.files.get_mut(&folder).unwrap().remove(&file);
                    rewrite(&mut tree, model.block(folder));
                }
            }
            // A folder made, sometimes with a folder in it.
            4 | 5 => {
                let made = tree.alloc();
                model.add_folder(made, folder);
                model.add_files(made, rng.below(20));
                let mut blocks = vec![made];
                if rng.below(3) == 0 {
                    let inner = tree.alloc();
                    model.add_folder(inner, made);
                    model.add_files(inner, rng.below(20));
                    blocks.push(inner);
                }
                tree.insert_after(folder, blocks.iter().map(|&f| model.block(f)).collect());
                tree.replace(model.block(folder));
                reshaped = true;
            }
            // A folder removed, with everything below it.
            6 | 7 => {
                let Some(parent) = model.parent[&folder] else {
                    continue;
                };
                for gone in model.subtree(folder) {
                    model.parent.remove(&gone);
                    model.files.remove(&gone);
                }
                tree.remove(folder);
                tree.replace(model.block(parent));
                reshaped = true;
            }
            // A folder moved elsewhere, not below itself.
            _ => {
                let Some(old) = model.parent[&folder] else {
                    continue;
                };
                let inside = model.subtree(folder);
                let targets: Vec<_> = folders
                    .iter()
                    .copied()
                    .filter(|f| !inside.contains(f) && *f != old)
                    .collect();
                if targets.is_empty() {
                    continue;
                }
                let to = rng.pick(&targets);
                let taken = tree.take(folder);
                model.parent.insert(folder, Some(to));
                let moved: Vec<Block> = taken.iter().map(|b| model.block(b.folder)).collect();
                tree.insert_after(to, moved);
                tree.replace(model.block(old));
                tree.replace(model.block(to));
                reshaped = true;
            }
        }
        assert_matches(&tree, &model);
        assert_eq!(
            tree.shape() != shape,
            reshaped,
            "seed {seed}, step {step}: the shape changes with the folders alone"
        );
        assert!(
            model.parent.len() < 2000,
            "seed {seed}, step {step}: the model grew past what a test needs"
        );
    }
}

/// With chunks of a few dozen entries, every edit cuts, merges or
/// rewrites some; a subtree is one range throughout.
#[test]
fn a_subtree_stays_one_range_while_chunks_are_rewritten() {
    let limits = Limits {
        target: 40,
        max: 60,
        min: 15,
    };
    // SKWIRD_TEST_SEEDS runs more: a longer search for a counterexample.
    let seeds = std::env::var("SKWIRD_TEST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    for seed in 1..=seeds {
        run(seed, 400, limits);
    }
}

/// With one chunk holding all, and with chunks of a single block.
#[test]
fn a_subtree_stays_one_range_at_the_limits_extremes() {
    run(
        99,
        300,
        Limits {
            target: 1_000_000,
            max: 1_000_000,
            min: 0,
        },
    );
    run(
        7,
        300,
        Limits {
            target: 1,
            max: 1,
            min: 0,
        },
    );
}

/// A folder holding more than a chunk's limit gets a chunk of its own,
/// and the folders around it stay in range.
#[test]
fn a_folder_larger_than_a_chunk_has_one_to_itself() {
    let limits = Limits {
        target: 40,
        max: 60,
        min: 15,
    };
    let mut model = Model::default();
    model.parent.insert(0, None);
    model.files.insert(0, BTreeSet::new());
    for id in 1..=3 {
        model.add_folder(id, 0);
        model.add_files(id, 10);
    }
    model.add_files(2, 500);
    let mut built = Vec::new();
    model.preorder(0, &mut built);
    let tree = Tree::build(built.iter().map(|&f| model.block(f)).collect(), limits);
    assert_matches(&tree, &model);
    let big = tree.loc(2).unwrap().chunk;
    assert_eq!(tree.chunk(big).len(), 1);
}
