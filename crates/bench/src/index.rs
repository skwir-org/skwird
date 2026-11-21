//! What skwird's index costs on a real tree (`docs/PLAN.md`, phase 1,
//! step 2): the crawl, the build, its size against the estimate, a save
//! and a load, and a few queries, each timed. It prints counts, sizes
//! and times, never a name or a path.

use std::path::{Path, PathBuf};
use std::time::Instant;

use skwird_crawl::{Cancel, Exclusions, Kind, Options, Root, Time, crawl_all};
use skwird_index::store;
use skwird_index::tree::Limits;
use skwird_index::{Index, NameQuery, Pattern, Query, build};

use crate::said::{bytes, count, time};

/// The highest memory the process held, from the kernel's count.
fn peak_memory() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1000)
}

fn name(text: &str) -> Query {
    Query {
        name: Some(NameQuery {
            pattern: Pattern::Skwir(text.to_owned()),
            case_sensitive: false,
        }),
        ..Query::default()
    }
}

/// The queries timed: broad and narrow names, a glob, kinds, sizes and a
/// time range. Synthetic terms, so that what is printed says nothing of
/// the tree's names beyond how many match.
fn queries() -> Vec<(&'static str, Query)> {
    let now = Time::now();
    let week_ago = Time {
        sec: now.sec - 7 * 86_400,
        nsec: 0,
    };
    vec![
        ("name \"e\"", name("e")),
        ("name \"report\"", name("report")),
        ("name \"readme\"", name("readme")),
        ("name \"zzqx\"", name("zzqx")),
        ("glob \"*.pdf\"", name("*.pdf")),
        ("glob \"*.rs\"", name("*.rs")),
        (
            "folders",
            Query {
                kinds: Some(vec![Kind::Folder]),
                ..Query::default()
            },
        ),
        (
            "1 MB and more",
            Query {
                size: Some(1_000_000..=u64::MAX),
                ..Query::default()
            },
        ),
        (
            "changed this week",
            Query {
                mtime: Some(week_ago..=now),
                ..Query::default()
            },
        ),
    ]
}

fn time_queries(index: &Index) {
    let Some(root) = index.root() else { return };
    println!("query                       time  hits");
    for (label, query) in queries() {
        let start = Instant::now();
        let hits = index.query(root, &query).map(|h| h.len());
        let took = start.elapsed();
        match hits {
            Ok(n) => println!("{label:<20} {:>12}  {}", time(took), count(n as u64)),
            Err(e) => println!("{label:<20} failed: {e}"),
        }
    }
}

pub fn run(path: &Path, threads: usize, keep: Option<PathBuf>) -> Result<(), String> {
    // The engine's default exclusions: a private folder's names are never
    // kept, not even in a measurement's index.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let (exclusions, _) = Exclusions::resolve(skwird_crawl::exclude::defaults(&home));
    let root = Root::open(path, exclusions).map_err(|e| e.to_string())?;
    let opts = Options {
        threads,
        ..Options::default()
    };
    let start = Instant::now();
    let crawl = crawl_all(&root, &opts, &Cancel::default()).map_err(|e| e.to_string())?;
    println!(
        "crawl: {} folders in {} on {threads} threads, every entry stat-ed",
        count(crawl.folders.len() as u64),
        time(start.elapsed())
    );
    let start = Instant::now();
    let mut index = build(crawl, Limits::default());
    println!("build: {}", time(start.elapsed()));
    let s = index.sizes();
    let total = s.chunk_bytes + s.names_bytes;
    println!(
        "index: {} entries in {} folders, {} distinct names, {} chunks",
        count(s.entries),
        count(s.folders),
        count(s.names),
        count(s.chunks)
    );
    println!(
        "size: {} ({} chunks, {} names), {:.1} bytes an entry",
        bytes(total),
        bytes(s.chunk_bytes),
        bytes(s.names_bytes),
        total as f64 / s.entries.max(1) as f64
    );
    // phase 0's estimate, every field at its full width.
    let name_bytes = s.names_bytes.saturating_sub(16 + s.names * 16);
    let estimate = s.entries * 33 + s.names * 16 + name_bytes + s.folders * 40;
    println!(
        "estimate at full widths: {} ({:.1} bytes an entry): packed is {:.0}% of it",
        bytes(estimate),
        estimate as f64 / s.entries.max(1) as f64,
        100.0 * total as f64 / estimate.max(1) as f64
    );
    time_queries(&index);
    let dir = keep.unwrap_or_else(|| {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(|| PathBuf::from("."));
        cache.join("skwird-bench").join("index")
    });
    let start = Instant::now();
    let now = Time::now();
    let mark = store::Mark {
        saved_at: now,
        crawled_at: now,
        proven_at: now,
        epoch: 0,
        log_seq: 0,
    };
    store::save(&mut index, &dir, &mark).map_err(|e| format!("cannot save: {e}"))?;
    println!("save: {}", time(start.elapsed()));
    drop(index);
    let start = Instant::now();
    let (loaded, _) = store::load(&dir, Limits::default()).map_err(|e| e.to_string())?;
    println!("load, mapped and checked: {}", time(start.elapsed()));
    time_queries(&loaded);
    if let Some(peak) = peak_memory() {
        println!("peak memory: {}", bytes(peak));
    }
    Ok(())
}
