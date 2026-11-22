//! What reading a folder again costs when a file of it is heard (DESIGN 8):
//! a tiny, a medium and a large folder, in an index of about 900,000
//! entries. It prints times alone. Ignored, since it builds a tree of its
//! own the first time; run with
//!
//! ```text
//! cargo test --release -p skwird-index --test reread_cost -- --ignored --nocapture
//! ```
//!
//! (`ONLY=tiny`, `medium` or `big` times one; `ROUNDS` sets how many.)
//!
//! The tree is kept in the user's cache between runs
//! (`~/.cache/skwird-tests/reread-cost`), and removed by hand.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use skwird_crawl::{Cancel, Exclusions, Options, Root};
use skwird_index::build::crawl_and_build;
use skwird_index::tree::Limits;
use skwird_index::update::reread_named;

/// This thread's processor time: what the machine's other work takes is
/// not counted.
fn thread_time() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}

/// The tree, made once: a folder of 60,000 files, 20 of 3,000, and
/// 100,000 of 8 in 100 folders of 1,000.
fn tree() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .expect("neither XDG_CACHE_HOME nor HOME is set")
        .join("skwird-tests/reread-cost");
    if base.join("made").exists() {
        return base;
    }
    if base.exists() {
        std::fs::remove_dir_all(&base).unwrap();
    }
    let files = |dir: &Path, n: usize| {
        std::fs::create_dir_all(dir).unwrap();
        for i in 0..n {
            std::fs::File::create(dir.join(format!("entry-{i:06}"))).unwrap();
        }
    };
    files(&base.join("big"), 60_000);
    for m in 0..20 {
        files(&base.join(format!("medium-{m:02}")), 3_000);
    }
    for a in 0..100 {
        for b in 0..1_000 {
            files(&base.join(format!("t{a:03}/{b:04}")), 8);
        }
    }
    std::fs::write(base.join("made"), b"").unwrap();
    base
}

#[test]
#[ignore = "builds a tree of 900,000 entries; run by hand"]
fn reading_a_folder_again_for_one_file() {
    let base = tree();
    let root = Root::open(&base, Exclusions::none()).unwrap();
    let opts = Options::default();
    let cancel = Cancel::default();
    let built = crawl_and_build(&root, &opts, &cancel, Limits::default()).unwrap();
    let mut index = built.index;
    root.let_go();
    let rounds: usize = std::env::var("ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(30);
    let only = std::env::var("ONLY").ok();
    for (label, rel) in [
        ("tiny", "t050/0500"),
        ("medium", "medium-07"),
        ("big", "big"),
    ] {
        if only.as_deref().is_some_and(|o| o != label) {
            continue;
        }
        let folder = index.folder_at(Path::new(rel)).unwrap();
        {
            let loc = index.tree().loc(folder).unwrap();
            let chunk = index.tree().chunk(loc.chunk);
            let view = chunk.view();
            let b = loc.block as usize;
            let start = thread_time();
            for _ in 0..200 {
                std::hint::black_box(view.block(b));
            }
            let runs = thread_time();
            for _ in 0..200 {
                std::hint::black_box(view.entries(b).map(|i| view.item(i)).collect::<Vec<_>>());
            }
            let one = thread_time();
            println!(
                "{label:6}: its block decoded {:?} a run at a time, its entries {:?} a value at a time",
                runs.saturating_sub(start) / 200,
                one.saturating_sub(runs) / 200
            );
        }
        let name = "entry-000003";
        let mut reads = Vec::with_capacity(rounds);
        let mut snapshots = Vec::with_capacity(rounds);
        for i in 0..rounds {
            std::fs::write(base.join(rel).join(name), i.to_string()).unwrap();
            let names: HashSet<Box<[u8]>> = [Box::from(name.as_bytes())].into();
            let start = thread_time();
            let done =
                reread_named(&mut index, &root, &opts, &cancel, &[(folder, Some(names))]).unwrap();
            root.let_go();
            let read = thread_time();
            let snapshot = index.clone();
            let cloned = thread_time();
            drop(snapshot);
            assert_eq!(done.reread, 1);
            reads.push(read.saturating_sub(start));
            snapshots.push(cloned.saturating_sub(read));
        }
        reads.sort_unstable();
        snapshots.sort_unstable();
        println!(
            "{label:6}: a read again {:?} at the median, {:?} at least; a snapshot {:?}",
            reads[rounds / 2],
            reads[0],
            snapshots[rounds / 2]
        );
    }
}
