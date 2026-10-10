//! What proving an index current costs where nothing watched (docs/DESIGN.md,
//! "Coverage"): every folder's change time, compared for equality with the
//! one the index stored. A folder's entries change only with its change
//! time, so the folders that differ are the only ones to read again.
//!
//! Two ways are timed: by each folder's whole path, and through its parent
//! folder held open (`statat` on each child), which costs one open per
//! parent but no lookup of the path above it.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, Mode, OFlags};

use crate::walk::{Ctime, Folder, ctime};

/// One run of a check.
pub struct Checked {
    pub took: Duration,
    /// Folders whose change time is not the one stored.
    pub changed: u64,
    /// Folders gone since (or no longer readable).
    pub gone: u64,
}

/// The walk's folders grouped by the folder holding them, for the check
/// through parents. The root, whose parent the walk did not read, is
/// checked by its path.
pub struct Groups {
    groups: Vec<(PathBuf, Children)>,
    root: Option<(PathBuf, Ctime)>,
}

/// A parent's folders, by name, with their change times.
type Children = Vec<(CString, Ctime)>;

impl Groups {
    pub fn of(list: &[Folder]) -> Groups {
        let mut by_parent: HashMap<&Path, Children> = HashMap::new();
        let mut root = None;
        for (i, folder) in list.iter().enumerate() {
            match (i, folder.path.parent(), folder.path.file_name()) {
                (1.., Some(parent), Some(name)) => by_parent
                    .entry(parent)
                    .or_default()
                    .push((CString::new(name.as_bytes()).unwrap(), folder.ctime)),
                _ => root = Some((folder.path.clone(), folder.ctime)),
            }
        }
        Groups {
            groups: by_parent
                .into_iter()
                .map(|(parent, children)| (parent.to_path_buf(), children))
                .collect(),
            root,
        }
    }
}

/// Stat every folder by its path, on `threads` threads.
pub fn by_path(list: &[Folder], threads: usize) -> Checked {
    let (changed, gone) = (AtomicU64::new(0), AtomicU64::new(0));
    let start = Instant::now();
    shared(list.len(), threads, |i| {
        let folder = &list[i];
        match rustix::fs::statat(rustix::fs::CWD, &folder.path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if ctime(&st) == folder.ctime => {}
            Ok(_) => _ = changed.fetch_add(1, Ordering::Relaxed),
            Err(_) => _ = gone.fetch_add(1, Ordering::Relaxed),
        }
    });
    Checked {
        took: start.elapsed(),
        changed: changed.into_inner(),
        gone: gone.into_inner(),
    }
}

/// Stat every folder through its parent held open, on `threads` threads.
pub fn by_parent(groups: &Groups, threads: usize) -> Checked {
    let (changed, gone) = (AtomicU64::new(0), AtomicU64::new(0));
    let start = Instant::now();
    if let Some((path, stored)) = &groups.root {
        match rustix::fs::statat(rustix::fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if ctime(&st) == *stored => {}
            Ok(_) => _ = changed.fetch_add(1, Ordering::Relaxed),
            Err(_) => _ = gone.fetch_add(1, Ordering::Relaxed),
        }
    }
    shared(groups.groups.len(), threads, |i| {
        let (parent, children) = &groups.groups[i];
        let Ok(fd) = rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) else {
            gone.fetch_add(children.len() as u64, Ordering::Relaxed);
            return;
        };
        for (name, stored) in children {
            match rustix::fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) if ctime(&st) == *stored => {}
                Ok(_) => _ = changed.fetch_add(1, Ordering::Relaxed),
                Err(_) => _ = gone.fetch_add(1, Ordering::Relaxed),
            }
        }
    });
    Checked {
        took: start.elapsed(),
        changed: changed.into_inner(),
        gone: gone.into_inner(),
    }
}

/// Run `each` on every index below `len`, shared by `threads` threads in
/// runs of 256.
fn shared(len: usize, threads: usize, each: impl Fn(usize) + Sync) {
    const RUN: usize = 256;
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let from = next.fetch_add(RUN, Ordering::Relaxed);
                    if from >= len {
                        break;
                    }
                    for i in from..(from + RUN).min(len) {
                        each(i);
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{Options, walk};

    /// Nothing changed is nothing found; a file added, a folder renamed or
    /// one removed are found by both checks, and only those.
    #[test]
    fn the_check_finds_the_folders_whose_entries_changed() {
        let tmp = crate::walk::tests::tempdir();
        let root = tmp.join("root");
        for dir in ["a/b", "c/d", "e"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let opts = Options {
            threads: 2,
            stat_files: false,
            cross_mounts: false,
        };
        let walked = walk(&root, &opts).unwrap();
        let groups = Groups::of(&walked.list);
        for threads in [1, 3] {
            let quiet = by_path(&walked.list, threads);
            assert_eq!((quiet.changed, quiet.gone), (0, 0));
            let quiet = by_parent(&groups, threads);
            assert_eq!((quiet.changed, quiet.gone), (0, 0));
        }
        // A file in a/b changes a/b; renaming c/d changes c, and c/d is gone;
        // removing e changes the root, and e is gone.
        std::fs::write(root.join("a/b/new.txt"), "").unwrap();
        std::fs::rename(root.join("c/d"), root.join("c/d2")).unwrap();
        std::fs::remove_dir(root.join("e")).unwrap();
        for threads in [1, 3] {
            let found = by_path(&walked.list, threads);
            assert_eq!((found.changed, found.gone), (3, 2), "a/b, c, root; c/d, e");
            let found = by_parent(&groups, threads);
            assert_eq!((found.changed, found.gone), (3, 2));
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
