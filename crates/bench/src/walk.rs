//! A walk of names, as skwird's crawl makes it (`skwird_crawl::crawl`,
//! the crawl that is built), counted: each folder read with `getdents64`,
//! each folder stat-ed for its identity and change time, and with
//! `--stat` every entry, as an index of sizes and times needs. Mounts
//! below the root are never crossed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use skwird_crawl::{Cancel, Crawled, Exclusions, Kind, Outcome, Root, Stored, StoredFolder, Why};

pub struct Options {
    pub threads: usize,
    /// Stat every entry, not only folders.
    pub stat_files: bool,
}

/// A change time: seconds and nanoseconds.
pub type Ctime = (i64, i64);

/// A stat's change time.
// Seconds and nanoseconds come as whatever types the system's `stat` has;
// the nanoseconds are below 10^9 in any of them.
#[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
pub fn ctime(st: &rustix::fs::Stat) -> Ctime {
    (st.st_ctime as i64, st.st_ctime_nsec as i64)
}

/// A folder read, with the change time the check compares.
pub struct Folder {
    pub path: PathBuf,
    pub ctime: Ctime,
}

/// What a walk found.
#[derive(Default)]
pub struct Walked {
    pub folders: u64,
    pub files: u64,
    pub links: u64,
    /// Pipes, sockets, devices.
    pub others: u64,
    /// The bytes of every name, each counted where it is.
    pub name_bytes: u64,
    /// Names told apart, and their bytes once each.
    pub distinct: u64,
    pub distinct_bytes: u64,
    /// Folders that could not be read (permission denied, gone meanwhile,
    /// a listing cut short).
    pub unreadable: u64,
    /// Folders where another mount starts, not walked into.
    pub mounts: u64,
    /// Folders holding a `.private` file, not walked into.
    pub private: u64,
    /// Every folder read, the root first, by its path.
    pub list: Vec<Folder>,
    /// The same folders as the check takes them.
    pub stored: Stored,
    pub took: Duration,
    /// The root filesystem's name.
    pub fs: String,
}

impl Walked {
    pub fn entries(&self) -> u64 {
        self.folders + self.files + self.links + self.others
    }
}

/// What the sink counts while the crawl runs.
#[derive(Default)]
struct Counts {
    walked: Walked,
    /// Names by their hash, with their length.
    names: HashMap<u64, u16>,
    /// Folders read, by slot, so that the root comes first.
    read: Vec<(usize, Folder, (PathBuf, StoredFolder))>,
}

/// The root, opened as the crawl opens it.
pub fn open(root: &Path) -> Result<Root, String> {
    Root::open(root, Exclusions::none()).map_err(|e| e.to_string())
}

pub fn walk(root: &Root, opts: &Options) -> Result<Walked, String> {
    let crawl_opts = skwird_crawl::Options {
        threads: opts.threads,
        attrs: opts.stat_files,
        ..skwird_crawl::Options::default()
    };
    let counts = Mutex::new(Counts::default());
    let took = skwird_crawl::crawl(
        root,
        Path::new(""),
        &crawl_opts,
        &Cancel::default(),
        &|slot, crawled| {
            // Counted on the crawl's thread, merged under the lock.
            let counted = count(root, slot, crawled);
            merge(counted, &mut counts.lock().unwrap());
        },
    )
    .map_err(|e| e.to_string())?;
    let mut counts = counts.into_inner().unwrap();
    counts.read.sort_unstable_by_key(|(slot, ..)| *slot);
    let mut walked = counts.walked;
    walked.folders += 1; // the root, which no listing names
    walked.distinct = counts.names.len() as u64;
    walked.distinct_bytes = counts.names.values().map(|&n| u64::from(n)).sum();
    for (_, folder, stored) in counts.read {
        walked.list.push(folder);
        walked.stored.push(&stored.0, stored.1);
    }
    walked.took = took;
    root.fs_name().clone_into(&mut walked.fs);
    Ok(walked)
}

/// One folder, counted.
#[derive(Default)]
struct Counted {
    walked: Walked,
    names: Vec<(u64, u16)>,
    read: Option<(usize, Folder, (PathBuf, StoredFolder))>,
}

fn count(root: &Root, slot: usize, crawled: Crawled) -> Counted {
    let mut counted = Counted::default();
    let listing = match crawled.outcome {
        Outcome::Listed(listing) => listing,
        Outcome::NotCovered(why) => {
            let w = &mut counted.walked;
            match why {
                Why::Mount | Why::NetworkMount => w.mounts += 1,
                Why::Private | Why::Excluded => w.private += 1,
                _ => w.unreadable += 1,
            }
            return counted;
        }
    };
    for (name, entry) in listing.iter() {
        let w = &mut counted.walked;
        match entry.kind {
            Kind::Folder => w.folders += 1,
            Kind::File => w.files += 1,
            Kind::Link => w.links += 1,
            Kind::Other => w.others += 1,
        }
        w.name_bytes += name.len() as u64;
        counted
            .names
            .push((fnv(name), name.len().min(usize::from(u16::MAX)) as u16));
    }
    let stat = listing.stat;
    counted.read = Some((
        slot,
        Folder {
            path: root.path().join(&crawled.rel),
            ctime: (stat.ctime.sec, i64::from(stat.ctime.nsec)),
        },
        (
            crawled.rel,
            StoredFolder {
                id: stat.id,
                ctime: stat.ctime,
                // The bench times the check itself: every folder is stat-ed.
                again: false,
            },
        ),
    ));
    counted
}

fn merge(counted: Counted, counts: &mut Counts) {
    let (w, c) = (&mut counts.walked, counted.walked);
    w.folders += c.folders;
    w.files += c.files;
    w.links += c.links;
    w.others += c.others;
    w.name_bytes += c.name_bytes;
    w.unreadable += c.unreadable;
    w.mounts += c.mounts;
    w.private += c.private;
    for (hash, len) in counted.names {
        counts.names.entry(hash).or_insert(len);
    }
    counts.read.extend(counted.read);
}

/// FNV-1a: names told apart by a 64-bit hash, cheap enough not to weigh
/// on the walk it is part of. Two names of 2 million share a hash with a
/// chance of about one in ten million.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests {
    use super::*;
    use skwird_testkit::Scratch;

    /// A tree of known shape is counted as it is: the root and its folders,
    /// files, links and names; a folder is listed once, with a change time.
    #[test]
    fn a_walk_counts_what_the_tree_holds() {
        let s = Scratch::new("bench-walk");
        s.dir("a/b");
        s.dir("c");
        s.file("a/same.txt");
        s.file("c/same.txt");
        s.file("a/b/other name.txt");
        s.link("link", "a");
        let root = open(s.path()).unwrap();
        for threads in [1, 4] {
            let opts = Options {
                threads,
                stat_files: threads == 4,
            };
            let walked = walk(&root, &opts).unwrap();
            assert_eq!(walked.folders, 4, "root, a, a/b, c");
            assert_eq!(walked.files, 3);
            assert_eq!(walked.links, 1);
            assert_eq!(walked.entries(), 8);
            assert_eq!(walked.list.len(), 4);
            assert_eq!(walked.list[0].path, s.path(), "the root first");
            // a, b, c, link, same.txt twice, other name.txt
            assert_eq!(walked.distinct, 6);
            assert_eq!(walked.distinct_bytes, 1 + 1 + 1 + 4 + 8 + 14);
            assert_eq!(walked.name_bytes, walked.distinct_bytes + 8);
            assert_eq!(walked.unreadable, 0);
        }
    }
}
