//! A walk of names, as skwird's crawl would make it: each folder read
//! with `getdents64` (rustix's `Dir`), each entry's kind from what the
//! listing says, a stat only for folders (their change time, which the
//! check compares, and their device, which tells a mount) and, with
//! `--stat`, for every entry, as an index of sizes and times needs.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

pub struct Options {
    pub threads: usize,
    /// Stat every entry, not only folders.
    pub stat_files: bool,
    /// Walk into other filesystems mounted below the root.
    pub cross_mounts: bool,
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

/// A folder found, with the change time the check compares.
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
    /// Folders that could not be read (permission denied, gone meanwhile).
    pub unreadable: u64,
    /// Folders of another filesystem, not walked into.
    pub mounts: u64,
    /// Every folder walked, the root first.
    pub list: Vec<Folder>,
    pub took: Duration,
    /// The root filesystem's type (`statfs`'s `f_type`).
    pub fs_type: u64,
}

impl Walked {
    pub fn entries(&self) -> u64 {
        self.folders + self.files + self.links + self.others
    }
}

/// One thread's share, merged when the walk ends.
#[derive(Default)]
struct Share {
    folders: u64,
    files: u64,
    links: u64,
    others: u64,
    name_bytes: u64,
    unreadable: u64,
    mounts: u64,
    /// Names by their hash, with their length.
    names: HashMap<u64, u16>,
    list: Vec<Folder>,
}

/// Folders waiting to be read, and how many are being read: the walk ends
/// when none waits and none is read.
struct Queue {
    state: Mutex<(Vec<PathBuf>, usize)>,
    turn: Condvar,
}

impl Queue {
    fn next(&self) -> Option<PathBuf> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(dir) = state.0.pop() {
                state.1 += 1;
                return Some(dir);
            }
            if state.1 == 0 {
                return None;
            }
            state = self.turn.wait(state).unwrap();
        }
    }

    fn done(&self, found: Vec<PathBuf>) {
        let mut state = self.state.lock().unwrap();
        state.1 -= 1;
        state.0.extend(found);
        self.turn.notify_all();
    }
}

pub fn walk(root: &Path, opts: &Options) -> Result<Walked, String> {
    let start = Instant::now();
    let fd = open(root).map_err(|e| format!("cannot open the root: {e}"))?;
    let own = rustix::fs::fstat(&fd).map_err(|e| format!("cannot stat the root: {e}"))?;
    let fs_type = rustix::fs::fstatfs(&fd).map_or(0, |s| s.f_type as u64);
    drop(fd);
    let queue = Queue {
        state: Mutex::new((vec![root.to_path_buf()], 0)),
        turn: Condvar::new(),
    };
    let shares: Vec<Share> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..opts.threads.max(1))
            .map(|_| scope.spawn(|| work(&queue, own.st_dev, opts)))
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    let took = start.elapsed();
    let mut walked = Walked {
        took,
        fs_type,
        folders: 1,
        ..Walked::default()
    };
    walked.list.push(Folder {
        path: root.to_path_buf(),
        ctime: ctime(&own),
    });
    let mut names: HashMap<u64, u16> = HashMap::new();
    for share in shares {
        walked.folders += share.folders;
        walked.files += share.files;
        walked.links += share.links;
        walked.others += share.others;
        walked.name_bytes += share.name_bytes;
        walked.unreadable += share.unreadable;
        walked.mounts += share.mounts;
        walked.list.extend(share.list);
        names.extend(share.names);
    }
    walked.distinct = names.len() as u64;
    walked.distinct_bytes = names.values().map(|&n| u64::from(n)).sum();
    Ok(walked)
}

fn open(dir: &Path) -> rustix::io::Result<rustix::fd::OwnedFd> {
    rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
}

fn work(queue: &Queue, root_dev: u64, opts: &Options) -> Share {
    let mut share = Share::default();
    while let Some(dir) = queue.next() {
        let mut found = Vec::new();
        read(&dir, root_dev, opts, &mut share, &mut found);
        queue.done(found);
    }
    share
}

/// Read one folder: count its entries, and queue its folders.
fn read(dir: &Path, root_dev: u64, opts: &Options, share: &mut Share, found: &mut Vec<PathBuf>) {
    let Ok(listing) = open(dir).and_then(Dir::new) else {
        share.unreadable += 1;
        return;
    };
    let mut listing = listing;
    let mut entries: Vec<(CString, FileType)> = Vec::new();
    while let Some(entry) = listing.read() {
        let Ok(entry) = entry else {
            share.unreadable += 1;
            break;
        };
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        entries.push((name.to_owned(), entry.file_type()));
    }
    let Ok(fd) = listing.fd() else { return };
    for (name, kind) in entries {
        let bytes = name.to_bytes();
        share.name_bytes += bytes.len() as u64;
        share
            .names
            .entry(fnv(bytes))
            .or_insert(bytes.len().min(usize::from(u16::MAX)) as u16);
        let stat = |name: &CStr| rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW);
        let kind = match kind {
            FileType::Unknown => match stat(&name) {
                Ok(st) => FileType::from_raw_mode(st.st_mode),
                Err(_) => FileType::Unknown,
            },
            kind => kind,
        };
        match kind {
            FileType::Directory => {
                share.folders += 1;
                let Ok(st) = stat(&name) else {
                    share.unreadable += 1;
                    continue;
                };
                if st.st_dev != root_dev && !opts.cross_mounts {
                    share.mounts += 1;
                    continue;
                }
                let path = dir.join(OsStr::from_bytes(bytes));
                share.list.push(Folder {
                    path: path.clone(),
                    ctime: ctime(&st),
                });
                found.push(path);
            }
            other => {
                match other {
                    FileType::RegularFile => share.files += 1,
                    FileType::Symlink => share.links += 1,
                    _ => share.others += 1,
                }
                if opts.stat_files {
                    let _ = stat(&name);
                }
            }
        }
    }
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

/// A filesystem's name by its `statfs` magic, or the number.
pub fn fs_name(magic: u64) -> String {
    match magic {
        0x9123_683e => "btrfs".into(),
        0xef53 => "ext4".into(),
        0x5846_5342 => "xfs".into(),
        0x2fc1_2fc1 => "zfs".into(),
        0xf2f5_2010 => "f2fs".into(),
        0xca45_1a4e => "bcachefs".into(),
        0x0102_1994 => "tmpfs".into(),
        0x794c_7630 => "overlayfs".into(),
        0x6573_5546 => "fuse".into(),
        0x6969 => "nfs".into(),
        0xff53_4d42 | 0xfe53_4d42 => "smb".into(),
        0 => "unknown".into(),
        other => format!("{other:#x}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tree of known shape is counted as it is: the root and its folders,
    /// files, links and names; a folder is listed once, with a change time.
    #[test]
    fn a_walk_counts_what_the_tree_holds() {
        let tmp = tempdir();
        let root = tmp.join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::write(root.join("a/same.txt"), "").unwrap();
        std::fs::write(root.join("c/same.txt"), "").unwrap();
        std::fs::write(root.join("a/b/other name.txt"), "").unwrap();
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        for threads in [1, 4] {
            let opts = Options {
                threads,
                stat_files: threads == 4,
                cross_mounts: false,
            };
            let walked = walk(&root, &opts).unwrap();
            assert_eq!(walked.folders, 4, "root, a, a/b, c");
            assert_eq!(walked.files, 3);
            assert_eq!(walked.links, 1);
            assert_eq!(walked.entries(), 8);
            assert_eq!(walked.list.len(), 4);
            // a, b, c, link, same.txt twice, other name.txt
            assert_eq!(walked.distinct, 6);
            assert_eq!(walked.distinct_bytes, 1 + 1 + 1 + 4 + 8 + 14);
            assert_eq!(walked.name_bytes, walked.distinct_bytes + 8);
            assert_eq!(walked.unreadable, 0);
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// A scratch folder of the test's own under the user's cache, not
    /// `/tmp`, which may be a tmpfs with other change-time rules.
    pub fn tempdir() -> PathBuf {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap()
            .join("skwird-tests");
        let dir = base.join(format!(
            "walk-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
