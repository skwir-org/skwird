//! Every query answers as a filter over a fresh walk would
//! (`docs/PLAN.md`, phase 1, step 2): names matched as skwir matches them,
//! kinds, sizes, modification times, below any folder; in one chunk or
//! many; built from a crawl, and saved and mapped back.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use globset::GlobBuilder;
use skwird_crawl::{Cancel, Exclusions, Kind, Options, Root, Time, crawl_all};
use skwird_index::store::{self, LoadError};
use skwird_index::tree::Limits;
use skwird_index::{Index, NameQuery, Pattern, Query, build};
use skwird_testkit::{Scratch, reference_walk};

/// What skwir's `NamePat` answers, written as it is.
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

fn at(secs: i64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.unsigned_abs())
}

/// A tree of awkward names, sizes and times.
fn tree(s: &Scratch) {
    s.write("Report 2026.PDF", &[1; 5000]);
    s.write("reports/q1 report.pdf", &[2; 120]);
    s.write("reports/old/report-1999.pdf", &vec![3; 70_000]);
    s.write("été/naïve 東京.md", b"unicode");
    s.file(Path::new(OsStr::from_bytes(b"latin1-\xe9t\xe9")));
    s.file(Path::new(OsStr::from_bytes(b"dir-\xff\xfe/report \xff")));
    s.file("\u{130}stanbul.jpg");
    s.file("\u{17f}ong s");
    s.file("ΟΔΟΣ.txt");
    s.write("data/file.bin", &[7; 1000]);
    s.hard_link("data/same.bin", "data/file.bin");
    s.link("to-data", "data");
    s.link("dangling", "nowhere");
    for i in 0..80 {
        s.write(format!("nest/n{i:02}/leaf {i}.txt"), &vec![1; i]);
    }
    for i in 0..1500 {
        s.write(format!("many/item-{i:04}.txt"), &vec![0; i % 37]);
    }
    for (name, secs) in [
        ("Report 2026.PDF", 1_700_000_000),
        ("data/file.bin", 86_400),
    ] {
        std::fs::File::options()
            .write(true)
            .open(s.join(name))
            .unwrap()
            .set_modified(at(secs))
            .unwrap();
    }
}

/// What a fresh walk holds: each entry's path below the root, kind,
/// inode, size and modification time.
type Walked = BTreeMap<PathBuf, (Kind, u64, u64, Time)>;

fn walk(root: &Path) -> Walked {
    reference_walk(root)
        .entries
        .into_iter()
        .map(|(path, seen)| {
            let kind = match seen.kind {
                skwird_testkit::Kind::File => Kind::File,
                skwird_testkit::Kind::Folder => Kind::Folder,
                skwird_testkit::Kind::Link => Kind::Link,
                skwird_testkit::Kind::Other => Kind::Other,
            };
            let modified = std::fs::symlink_metadata(root.join(&path))
                .unwrap()
                .modified()
                .unwrap();
            let since = modified.duration_since(UNIX_EPOCH).unwrap();
            let mtime = Time {
                sec: i64::try_from(since.as_secs()).unwrap(),
                nsec: since.subsec_nanos(),
            };
            (path, (kind, seen.ino, seen.size, mtime))
        })
        .collect()
}

/// The queries tried: names, kinds, sizes, times, and each mixed.
fn queries() -> Vec<Query> {
    let name = |t: &str, cs| {
        Some(NameQuery {
            pattern: Pattern::Skwir(t.to_owned()),
            case_sensitive: cs,
        })
    };
    let mut out = vec![Query::default()];
    for term in [
        "report",
        "REPORT",
        ".pdf",
        "*.pdf",
        "*.PDF",
        "rep*",
        "i",
        "s",
        "*s*",
        "ς",
        "\u{fffd}",
        "item-14",
        "item-1???.txt",
        "a[b",
        "",
        "zzz",
    ] {
        for cs in [false, true] {
            out.push(Query {
                name: name(term, cs),
                ..Query::default()
            });
        }
    }
    for kinds in [
        vec![Kind::File],
        vec![Kind::Folder],
        vec![Kind::Link, Kind::Other],
    ] {
        out.push(Query {
            kinds: Some(kinds),
            ..Query::default()
        });
    }
    for size in [0..=0, 1..=100, 1000..=1000, 4000..=u64::MAX] {
        out.push(Query {
            size: Some(size),
            ..Query::default()
        });
    }
    let t = |sec| Time { sec, nsec: 0 };
    for range in [
        t(0)..=t(100_000),
        t(1_600_000_000)..=t(1_800_000_000),
        t(-5)..=t(-1),
    ] {
        out.push(Query {
            mtime: Some(range),
            ..Query::default()
        });
    }
    out.push(Query {
        name: name("report", false),
        kinds: Some(vec![Kind::File]),
        size: Some(100..=10_000),
        ..Query::default()
    });
    out
}

/// What the walk holds below `root` that answers `query`.
fn filtered(walked: &Walked, root: &Path, query: &Query) -> Vec<PathBuf> {
    walked
        .iter()
        .filter(|(path, _)| path.starts_with(root) && *path != root)
        .filter(|&(path, &(kind, _, size, mtime))| {
            let name = path.file_name().unwrap().as_bytes();
            query.name.as_ref().is_none_or(|q| {
                let Pattern::Skwir(text) = &q.pattern else {
                    unreachable!()
                };
                skwir_matches(text, q.case_sensitive, name)
            }) && query.kinds.as_ref().is_none_or(|ks| ks.contains(&kind))
                && query.size.as_ref().is_none_or(|r| r.contains(&size))
                && query.mtime.as_ref().is_none_or(|r| r.contains(&mtime))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn answered(index: &Index, root: &Path, query: &Query) -> Vec<PathBuf> {
    let folder = index
        .folder_at(root)
        .expect("the root is a folder of the index");
    let mut paths: Vec<PathBuf> = index
        .query(folder, query)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect();
    paths.sort();
    paths
}

fn indexed(s: &Scratch, limits: Limits) -> Index {
    let root = Root::open(s.path(), Exclusions::none()).unwrap();
    let crawl = crawl_all(&root, &Options::default(), &Cancel::default()).unwrap();
    build(crawl, limits)
}

fn roots() -> Vec<PathBuf> {
    [
        "",
        "reports",
        "reports/old",
        "many",
        "data",
        "nest",
        "nest/n07",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

fn assert_answers_as_the_walk(index: &Index, walked: &Walked) {
    for root in roots() {
        for query in queries() {
            assert_eq!(
                answered(index, &root, &query),
                filtered(walked, &root, &query),
                "below {}: {query:?}",
                root.display()
            );
        }
    }
}

#[test]
fn every_query_answers_as_a_filter_over_a_fresh_walk() {
    let s = Scratch::new("index-queries");
    tree(&s);
    let walked = walk(s.path());
    let one = indexed(&s, Limits::default());
    assert_eq!(one.sizes().chunks, 1);
    assert_answers_as_the_walk(&one, &walked);
    let many = indexed(
        &s,
        Limits {
            target: 40,
            max: 60,
            min: 15,
        },
    );
    assert!(many.sizes().chunks > 5, "{:?}", many.sizes());
    assert_answers_as_the_walk(&many, &walked);
}

/// Each hit carries what the walk says of it: its kind, identity and
/// attributes; two names of one file have one identity.
#[test]
fn a_hit_carries_its_entrys_identity_and_attributes() {
    let s = Scratch::new("index-hits");
    tree(&s);
    let walked = walk(s.path());
    let index = indexed(&s, Limits::default());
    let hits = index
        .query(index.root().unwrap(), &Query::default())
        .unwrap();
    assert_eq!(hits.len(), walked.len());
    for hit in &hits {
        let (kind, ino, size, mtime) = walked[&hit.path];
        assert_eq!((hit.kind, hit.id.ino), (kind, ino), "{:?}", hit.path);
        let attrs = hit.attrs.expect("every entry was stat-ed");
        assert_eq!((attrs.size, attrs.mtime), (size, mtime), "{:?}", hit.path);
    }
    let id = |p: &str| hits.iter().find(|h| h.path == Path::new(p)).unwrap().id;
    assert_eq!(id("data/file.bin"), id("data/same.bin"));
    for (rel, (kind, ..)) in &walked {
        if *kind == Kind::Folder {
            let folder = index.folder_at(rel).unwrap();
            assert_eq!(&index.path_of(folder), rel);
        }
    }
    assert!(
        index.folder_at(Path::new("to-data")).is_none(),
        "a link is no folder"
    );
    assert!(index.folder_at(Path::new("Report 2026.PDF")).is_none());
}

/// Saved, then mapped back, it answers alike; its files are its user's
/// alone; damaged, of another layout or missing, it is refused, to be
/// rebuilt.
#[test]
fn saved_and_mapped_back_it_answers_alike() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("index-store");
    tree(&s);
    let walked = walk(s.path());
    let limits = Limits {
        target: 200,
        max: 300,
        min: 50,
    };
    let mut index = indexed(&s, limits);
    let dir = s.join("cache/skwird/index");
    let saved_at = Time {
        sec: 1_790_000_000,
        nsec: 7,
    };
    store::save(&mut index, &dir, saved_at).unwrap();
    assert!(index.tree().chunks().all(|(_, c)| c.is_mapped()));
    assert_answers_as_the_walk(
        &index,
        &walked
            .clone()
            .into_iter()
            .filter(|(p, _)| !p.starts_with("cache"))
            .collect(),
    );
    let (loaded, when) = store::load(&dir, limits).unwrap();
    assert_eq!(when, saved_at);
    assert_eq!(loaded.sizes(), index.sizes());
    let walked: Walked = walked
        .into_iter()
        .filter(|(p, _)| !p.starts_with("cache"))
        .collect();
    assert_answers_as_the_walk(&loaded, &walked);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    for entry in std::fs::read_dir(&dir).unwrap() {
        assert_eq!(mode(&entry.unwrap().path()), 0o600);
    }
    // Saved again unchanged, it writes no chunk and keeps its files.
    let before: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut again = loaded;
    store::save(&mut again, &dir, saved_at).unwrap();
    let after: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before.len(), after.len());
    // A chunk damaged.
    let chunk = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("chunk-")
        })
        .unwrap();
    let mut bytes = std::fs::read(&chunk).unwrap();
    bytes.truncate(bytes.len() / 2);
    std::fs::write(&chunk, bytes).unwrap();
    assert!(matches!(store::load(&dir, limits), Err(LoadError::Damaged)));
    // A manifest of another layout.
    let manifest = dir.join("manifest");
    let mut bytes = std::fs::read(&manifest).unwrap();
    bytes[4] = 99;
    std::fs::write(&manifest, bytes).unwrap();
    assert!(matches!(store::load(&dir, limits), Err(LoadError::Layout)));
    assert!(matches!(
        store::load(&s.join("nowhere"), limits),
        Err(LoadError::Missing)
    ));
}
