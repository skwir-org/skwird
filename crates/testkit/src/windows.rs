//! The test kit on Windows (`docs/PLAN.md`, phase 4): scratch folders
//! under the user's local application data (under Wine, in the tests'
//! prefix, `tools/wine-tests`), and a reference walk through the standard
//! library's listing, each entry's identity asked of the system.

#![allow(unsafe_code)] // A file's identity and the loader have no safe binding; each block says why it is sound.

use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, FileIdInfo,
    GetFileInformationByHandleEx,
};

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::{Kind, Seen, Walk};

/// A folder of the test's own, removed when it is dropped, with what a
/// test needs to build a tree in it. Every path a method takes is
/// relative to it.
pub struct Scratch {
    root: PathBuf,
}

impl Scratch {
    /// A new, empty folder, named after `label`, the process and a count.
    pub fn new(label: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .expect("LOCALAPPDATA is not set")
            .join("skwird-tests");
        let root = base.join(format!(
            "{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(verbatim(&root));
        std::fs::create_dir_all(&root).expect("cannot make the scratch folder");
        // Named as the crawl names it: resolved, and plainly.
        let root = plain(&std::fs::canonicalize(&root).expect("cannot resolve the scratch folder"));
        Scratch { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel)
    }

    /// A folder, and the folders above it.
    pub fn dir(&self, rel: impl AsRef<Path>) -> PathBuf {
        let path = self.join(rel);
        std::fs::create_dir_all(verbatim(&path)).expect("cannot make a folder");
        path
    }

    /// An empty file, and the folders above it.
    pub fn file(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.write(rel, b"")
    }

    /// A file holding `bytes`, and the folders above it.
    pub fn write(&self, rel: impl AsRef<Path>, bytes: &[u8]) -> PathBuf {
        let path = self.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(verbatim(parent)).expect("cannot make a folder");
        }
        std::fs::write(verbatim(&path), bytes).expect("cannot write a file");
        path
    }

    /// A symbolic link at `rel` to `target`, as written: a folder's link
    /// where it leads to a folder, a file's otherwise.
    pub fn link(&self, rel: impl AsRef<Path>, target: impl AsRef<Path>) -> PathBuf {
        let path = self.join(rel);
        let target = target.as_ref();
        let leads = path
            .parent()
            .map_or(target.to_path_buf(), |p| p.join(target));
        let made = if leads.is_dir() {
            std::os::windows::fs::symlink_dir(target, &path)
        } else {
            std::os::windows::fs::symlink_file(target, &path)
        };
        made.expect("cannot make a link");
        path
    }

    /// A second name at `rel` for the file at `existing`.
    pub fn hard_link(&self, rel: impl AsRef<Path>, existing: impl AsRef<Path>) -> PathBuf {
        let path = self.join(rel);
        std::fs::hard_link(self.join(existing), &path).expect("cannot make a hard link");
        path
    }

    /// `levels` folders named `name`, each in the one before, below
    /// `rel`, and a file named `bottom` in the last: past the 260
    /// characters a plain path takes. Gives the last folder's path,
    /// relative to the scratch folder.
    pub fn deep(&self, rel: impl AsRef<Path>, levels: usize, name: &str) -> PathBuf {
        let mut deepest = rel.as_ref().to_path_buf();
        for _ in 0..levels {
            deepest.push(name);
        }
        self.file(deepest.join("bottom"));
        deepest
    }

    /// Remove the file, link or tree at `rel`.
    pub fn remove(&self, rel: impl AsRef<Path>) {
        let path = verbatim(&self.join(rel));
        match std::fs::symlink_metadata(&path) {
            // A link to a folder is removed as a folder, never followed.
            Ok(meta)
                if meta.is_dir() || meta.is_symlink() && std::fs::remove_dir(&path).is_ok() =>
            {
                let _ = std::fs::remove_dir_all(&path);
            }
            Ok(_) => std::fs::remove_file(&path).expect("cannot remove a file"),
            Err(_) => {}
        }
    }

    /// Rename `from` to `to`, both relative.
    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) {
        std::fs::rename(self.join(from), self.join(to)).expect("cannot rename");
    }

    /// Set the modification time of the folder or file at `rel` to `to`,
    /// as `tar` does.
    pub fn set_modified(&self, rel: impl AsRef<Path>, to: std::time::SystemTime) {
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(verbatim(&self.join(rel)))
            .expect("cannot open")
            .set_modified(to)
            .expect("cannot set a time");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(verbatim(&self.root));
    }
}

/// Whether this is a Windows machine of the tests' own, thrown away
/// after them: a runner GitHub hosts (`.github/workflows/windows.yml`),
/// which sets `SKWIRD_TEST_MACHINE`. A test that changes the system (a
/// task registered, a volume made) or needs an administrator runs there
/// alone, as Linux's root tests run in their machine (AGENTS.md,
/// Testing), and is skipped anywhere else: never on a developer's own
/// system. `SKWIRD_TEST_MACHINE` set anywhere else stops the test.
pub fn throwaway() -> bool {
    let asked = std::env::var_os("SKWIRD_TEST_MACHINE").is_some();
    // GitHub's own runners say so; a runner of someone's own says
    // `self-hosted`, and may be their machine.
    let hosted = std::env::var("RUNNER_ENVIRONMENT").is_ok_and(|v| v == "github-hosted");
    assert!(
        !asked || (hosted && !under_wine()),
        "SKWIRD_TEST_MACHINE is set, but this is not a runner GitHub hosts"
    );
    asked
}

/// Whether this runs under Wine, not Windows (`tools/wine-tests`): what
/// Wine cannot hold, a test does not ask of it, and says so.
pub fn under_wine() -> bool {
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    // SAFETY: a NUL-terminated name; ntdll is loaded in every process.
    let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
    if module.is_null() {
        return false;
    }
    // SAFETY: a NUL-terminated name, of a module loaded; nothing is called.
    unsafe { GetProcAddress(module, c"wine_get_version".as_ptr().cast()) }.is_some()
}

/// `path` named plainly: `C:\Users` for `\\?\C:\Users`.
fn plain(path: &Path) -> PathBuf {
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::VerbatimDisk(d) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(d)));
                out.extend(parts.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// `path` in its verbatim form, which takes any length: rebuilt from its
/// components, since a verbatim path takes `\` alone as a separator.
fn verbatim(path: &Path) -> PathBuf {
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) => {
                let mut out = PathBuf::from(format!(r"\\?\{}:\", char::from(d)));
                out.extend(parts.filter(|c| matches!(c, Component::Normal(_))));
                out
            }
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// The identity of the entry at `path` (itself, never what it links
/// to), and whether it is a reparse point: its volume's serial and file
/// id, as the crawl takes them.
fn identity(path: &Path) -> Option<(u64, u64)> {
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(verbatim(path))
        .ok()?;
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::zeroed();
    // SAFETY: the buffer is the structure's size, the size given; made of
    // integers, it is valid whatever the call writes, or zeroed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).ok()?,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: zeroed, then written by the call.
    let info = unsafe { info.assume_init() };
    let mut low = [0; 8];
    low.copy_from_slice(&info.FileId.Identifier[..8]);
    Some((info.VolumeSerialNumber, u64::from_le_bytes(low)))
}

/// Walk `root` the plain way: one folder after the other through the
/// standard library's listing, each entry's identity asked of it, never
/// into a reparse point, nor another volume. Nothing clever, so that
/// skwird's crawl can be held to it.
pub fn reference_walk(root: &Path) -> Walk {
    let (volume, _) = identity(root).expect("cannot stat the root");
    let mut walk = Walk::default();
    walk_into(root, Path::new(""), volume, &mut walk);
    walk
}

fn walk_into(dir: &Path, rel: &Path, volume: u64, walk: &mut Walk) {
    let Ok(listing) = std::fs::read_dir(verbatim(dir)) else {
        walk.not_read.insert(rel.to_path_buf());
        return;
    };
    for entry in listing {
        let Ok(entry) = entry else {
            walk.not_read.insert(rel.to_path_buf());
            return;
        };
        let name = entry.file_name();
        let path = rel.join(&name);
        let full = dir.join(&name);
        // The listing's own facts: a link's, never its target's.
        let Ok(meta) = entry.metadata() else { continue };
        let reparse = std::os::windows::fs::MetadataExt::file_attributes(&meta)
            & FILE_ATTRIBUTE_REPARSE_POINT
            != 0;
        let kind = if reparse {
            Kind::Link
        } else if meta.is_dir() {
            Kind::Folder
        } else {
            Kind::File
        };
        let Some((dev, ino)) = identity(&full) else {
            continue;
        };
        walk.entries.insert(
            path.clone(),
            Seen {
                kind,
                dev,
                ino,
                size: meta.len(),
            },
        );
        if kind != Kind::Folder {
            continue;
        }
        if dev != volume {
            walk.not_read.insert(path);
            continue;
        }
        walk_into(&full, &path, volume, walk);
    }
}
