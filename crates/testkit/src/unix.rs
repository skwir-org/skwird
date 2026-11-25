//! The test kit on Unix: scratch folders made through handles, a
//! reference walk through `getdents`, namespaces, the root machine.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

use crate::{Kind, Seen, Walk};

/// A folder of the test's own, removed when it is dropped, with what a
/// test needs to build a tree in it. Every path a method takes is
/// relative to it.
pub struct Scratch {
    root: PathBuf,
    /// Mounts made in it, let go of before it is removed.
    mounts: RefCell<Vec<PathBuf>>,
}

impl Scratch {
    /// A new, empty folder, named after `label`, the process and a count.
    pub fn new(label: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .expect("neither XDG_CACHE_HOME nor HOME is set")
            .join("skwird-tests");
        let root = base.join(format!(
            "{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if root.exists() {
            remove_tree(&root);
        }
        std::fs::create_dir_all(&root).expect("cannot make the scratch folder");
        // The walk opens folders without following links: the scratch
        // folder is named as the system resolves it.
        let root = std::fs::canonicalize(&root).expect("cannot resolve the scratch folder");
        Scratch {
            root,
            mounts: RefCell::new(Vec::new()),
        }
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
        std::fs::create_dir_all(&path).expect("cannot make a folder");
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
            std::fs::create_dir_all(parent).expect("cannot make a folder");
        }
        std::fs::write(&path, bytes).expect("cannot write a file");
        path
    }

    /// A symbolic link at `rel` to `target`, as written.
    pub fn link(&self, rel: impl AsRef<Path>, target: impl AsRef<Path>) -> PathBuf {
        let path = self.join(rel);
        std::os::unix::fs::symlink(target, &path).expect("cannot make a link");
        path
    }

    /// A second name at `rel` for the file at `existing`.
    pub fn hard_link(&self, rel: impl AsRef<Path>, existing: impl AsRef<Path>) -> PathBuf {
        let path = self.join(rel);
        std::fs::hard_link(self.join(existing), &path).expect("cannot make a hard link");
        path
    }

    /// `levels` folders named `name`, each in the one before, below
    /// `rel`, and a file named `bottom` in the last. Made through each
    /// folder's handle, so the path may be longer than the system allows.
    /// Gives the last folder's path, relative to the scratch folder.
    pub fn deep(&self, rel: impl AsRef<Path>, levels: usize, name: &str) -> PathBuf {
        let rel = rel.as_ref();
        let mut fd = open_dir(&self.dir(rel)).expect("cannot open a folder");
        let mut deepest = rel.to_path_buf();
        for _ in 0..levels {
            rustix::fs::mkdirat(&fd, name, Mode::from_raw_mode(0o755))
                .expect("cannot make a deep folder");
            fd = rustix::fs::openat(
                &fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("cannot open a deep folder");
            deepest.push(name);
        }
        rustix::fs::openat(
            &fd,
            "bottom",
            OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .expect("cannot make the deepest file");
        deepest
    }

    /// A btrfs subvolume at `rel`, or `false` where none can be made
    /// (another filesystem, no `btrfs` command).
    pub fn subvolume(&self, rel: impl AsRef<Path>) -> bool {
        Command::new("btrfs")
            .args(["subvolume", "create"])
            .arg(self.join(rel))
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// Take every permission from the folder or file at `rel`.
    pub fn deny(&self, rel: impl AsRef<Path>) {
        set_mode(&self.join(rel), Mode::empty());
    }

    /// Give a folder at `rel` its usual permissions back.
    pub fn allow(&self, rel: impl AsRef<Path>) {
        set_mode(&self.join(rel), Mode::from_raw_mode(0o755));
    }

    /// Remove the file, link or tree at `rel`.
    pub fn remove(&self, rel: impl AsRef<Path>) {
        let path = self.join(rel);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => remove_tree(&path),
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
        std::fs::File::open(self.join(rel))
            .expect("cannot open")
            .set_modified(to)
            .expect("cannot set a time");
    }

    /// A tmpfs mounted at `rel`, a folder made first. Only in a
    /// namespace of the test's own ([`in_namespace`]).
    pub fn mount_tmpfs(&self, rel: impl AsRef<Path>) -> PathBuf {
        let at = self.dir(rel);
        run_mount(&["-t", "tmpfs", "skwird-test"], &at);
        self.mounts.borrow_mut().push(at.clone());
        at
    }

    /// The folder `from` mounted again at `to`, a folder made first.
    /// Only in a namespace of the test's own ([`in_namespace`]).
    pub fn mount_bind(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> PathBuf {
        let at = self.dir(to);
        let from = self.join(from);
        run_mount(
            &["--bind", from.to_str().expect("a scratch path is UTF-8")],
            &at,
        );
        self.mounts.borrow_mut().push(at.clone());
        at
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for at in self.mounts.borrow().iter().rev() {
            let _ = Command::new("umount").arg("-l").arg(at).status();
        }
        remove_tree(&self.root);
    }
}

fn run_mount(args: &[&str], at: &Path) {
    let out = Command::new("mount")
        .args(args)
        .arg(at)
        .output()
        .expect("cannot run mount");
    assert!(
        out.status.success(),
        "mount failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn set_mode(path: &Path, mode: Mode) {
    rustix::fs::chmod(path, mode).expect("cannot change a mode");
}

fn open_dir(path: &Path) -> rustix::io::Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Remove a tree whatever it holds: folders a test denied are given
/// their permissions back first, through their parents' handles, so
/// depth does not matter.
fn remove_tree(path: &Path) {
    fn allow_all(dir: &OwnedFd) {
        let Ok(mut listing) = Dir::read_from(dir) else {
            return;
        };
        let mut folders = Vec::new();
        while let Some(Ok(entry)) = listing.read() {
            let name = entry.file_name().to_owned();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            if entry.file_type() == FileType::Directory {
                folders.push(name);
            }
        }
        for name in folders {
            let _ = rustix::fs::chmodat(dir, &name, Mode::from_raw_mode(0o755), AtFlags::empty());
            if let Ok(child) = rustix::fs::openat(
                dir,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                allow_all(&child);
            }
        }
    }
    let _ = rustix::fs::chmod(path, Mode::from_raw_mode(0o755));
    if let Ok(fd) = open_dir(path) {
        allow_all(&fd);
    }
    let _ = std::fs::remove_dir_all(path);
}

/// Walk `root` the plain way: one folder after the other, each opened
/// through its parent's handle without following a link, each entry
/// stat-ed without following one, never into another mount (told by its
/// mount id). Nothing clever, so that skwird's crawl can be held to it.
pub fn reference_walk(root: &Path) -> Walk {
    let fd = open_dir(root).expect("cannot open the root");
    let mount = mount_id(&fd, None).expect("cannot stat the root");
    let mut walk = Walk::default();
    walk_into(&fd, Path::new(""), mount, &mut walk);
    walk
}

fn walk_into(dir: &OwnedFd, rel: &Path, mount: u64, walk: &mut Walk) {
    let Ok(mut listing) = Dir::read_from(dir) else {
        walk.not_read.insert(rel.to_path_buf());
        return;
    };
    let mut names = Vec::new();
    while let Some(entry) = listing.read() {
        let Ok(entry) = entry else {
            walk.not_read.insert(rel.to_path_buf());
            return;
        };
        let name = entry.file_name().to_owned();
        if !matches!(name.to_bytes(), b"." | b"..") {
            names.push(name);
        }
    }
    for name in names {
        let path = rel.join(OsStr::from_bytes(name.to_bytes()));
        let Ok(st) = rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        let kind = match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Folder,
            FileType::Symlink => Kind::Link,
            _ => Kind::Other,
        };
        #[allow(clippy::unnecessary_cast)] // the types differ between systems
        walk.entries.insert(
            path.clone(),
            Seen {
                kind,
                dev: device(st.st_dev),
                ino: st.st_ino as u64,
                size: st.st_size as u64,
            },
        );
        if kind != Kind::Folder {
            continue;
        }
        if mount_id(dir, Some(&name)).ok() != Some(mount) {
            walk.not_read.insert(path);
            continue;
        }
        match rustix::fs::openat(
            dir,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(child) => walk_into(&child, &path, mount, walk),
            Err(_) => _ = walk.not_read.insert(path),
        }
    }
}

/// A device number, as the crawl keeps it: macOS gives it signed, and its
/// bits are taken as they are.
#[cfg(target_os = "macos")]
#[allow(clippy::cast_sign_loss)] // a device number's bits
fn device(dev: Device) -> u64 {
    u64::from(dev as u32)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_cast)] // the types differ between systems
fn device(dev: Device) -> u64 {
    dev as u64
}

/// The type of a stat's device number.
type Device = rustix::fs::Dev;

/// The mount a folder is in: its mount id on Linux, its device elsewhere.
#[cfg(target_os = "linux")]
fn mount_id(dir: &impl AsFd, name: Option<&std::ffi::CStr>) -> rustix::io::Result<u64> {
    use rustix::fs::StatxFlags;
    let (name, flags) = name.map_or((c"", AtFlags::EMPTY_PATH), |name| {
        (name, AtFlags::SYMLINK_NOFOLLOW)
    });
    let st = rustix::fs::statx(dir, name, flags, StatxFlags::MNT_ID)?;
    Ok(st.stx_mnt_id)
}

/// The mount a folder is in: its mount id on Linux, its device elsewhere.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_cast)] // the types differ between systems
fn mount_id(dir: &impl AsFd, name: Option<&std::ffi::CStr>) -> rustix::io::Result<u64> {
    let st = match name {
        Some(name) => rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?,
        None => rustix::fs::fstat(dir)?,
    };
    Ok(device(st.st_dev))
}

/// Whether this run of the calling test is the one in a user and mount
/// namespace of its own, where it is root and may mount.
///
/// The first time a test calls it, the test's binary is run again with
/// only that test, under `unshare --user --map-root-user --mount`, and
/// this gives `false` once that run passed (it panics with the run's
/// output when it failed). In that run it gives `true`, and the test goes
/// on. Where no namespace can be made, it says why and gives `false`, and
/// the test is skipped. The test must run on a thread named after it, as
/// the test harness names them.
pub fn in_namespace() -> bool {
    const INSIDE: &str = "SKWIRD_TEST_IN_NAMESPACE";
    if std::env::var_os(INSIDE).is_some() {
        return true;
    }
    let current = std::thread::current();
    let test = current
        .name()
        .expect("a test runs on a thread named after it");
    let exe = std::env::current_exe().expect("cannot find the test's binary");
    let out = match Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--"])
        .arg(exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(INSIDE, "1")
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            eprintln!("skipped: no namespace ({e})");
            return false;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && stderr.contains("unshare failed") {
        eprintln!("skipped: no namespace ({})", stderr.trim());
        return false;
    }
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "the test failed in its namespace:\n{stdout}\n{stderr}"
    );
    false
}

/// The machine the tests that need root run in (`tools/vm/root.nix`,
/// AGENTS.md, Testing): filesystems of its own, which a test may mark,
/// mount and fill as root.
pub struct Machine {
    /// An ext4 filesystem, mounted.
    pub ext4: PathBuf,
    /// A btrfs filesystem mounted through a subvolume (`home`), as a
    /// system's homes often are, holding two subvolumes of its own (`a`
    /// and `b`).
    pub btrfs: PathBuf,
    /// An ext4 filesystem on a device whose every read waits 20 ms, as a
    /// spinning disk's seek does (dm-delay): what is read from it cold is
    /// slow ([`Machine::cold`]).
    pub slow: PathBuf,
    slow_device: PathBuf,
    /// A USB stick QEMU emulates, removable, its FAT filesystem made whole
    /// on the device and labelled `TEST STICK`, mounted nowhere: the
    /// device, for a test to mount.
    pub stick: PathBuf,
    /// A second, its btrfs filesystem made whole on the device and
    /// labelled `TESTBTRFS`, mounted nowhere: btrfs's mounts have device
    /// numbers of their own, never their device's.
    pub stick_btrfs: PathBuf,
}

impl Machine {
    /// What is written made durable, and the caches dropped: what is read
    /// next comes from the devices, from [`Machine::slow`]'s slowly. That
    /// one is mounted again: what its journal holds of blocks just written,
    /// dropping the caches does not let go of.
    pub fn cold(&self) {
        rustix::fs::sync();
        let run = |cmd: &mut std::process::Command| {
            let done = cmd.status().expect("cannot run mount");
            assert!(done.success(), "{cmd:?}: {done}");
        };
        run(std::process::Command::new("umount").arg(&self.slow));
        run(std::process::Command::new("mount")
            .arg(&self.slow_device)
            .arg(&self.slow));
        std::fs::write("/proc/sys/vm/drop_caches", b"3\n").expect("cannot drop the caches");
    }
}

/// Where the machine names its filesystems.
const MACHINE: &str = "/etc/skwird-test-machine";

/// The test machine, when this runs in it as root; otherwise `None`,
/// having said so, and the test is skipped: a test that needs root runs
/// in the machine, never on a developer's own system, even under sudo.
///
/// The machine sets `SKWIRD_TEST_MACHINE` for its runs, so that a test
/// that would skip there fails instead.
pub fn machine() -> Option<Machine> {
    let inside = std::env::var_os("SKWIRD_TEST_MACHINE").is_some();
    let named = std::fs::read_to_string(MACHINE).ok();
    let root = rustix::process::geteuid().is_root();
    let Some(named) = named.filter(|_| root) else {
        assert!(
            !inside,
            "SKWIRD_TEST_MACHINE is set, but this is not the test machine as root"
        );
        eprintln!("skipped: needs root in the test machine (tools/vm/root-tests)");
        return None;
    };
    let path = |key: &str| {
        named
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map_or_else(|| panic!("{MACHINE} names no {key}"), PathBuf::from)
    };
    Some(Machine {
        ext4: path("ext4"),
        btrfs: path("btrfs"),
        slow: path("slow"),
        slow_device: path("slow_device"),
        stick: path("stick"),
        stick_btrfs: path("stick_btrfs"),
    })
}

/// A folder of a root test's own on one of the machine's filesystems
/// ([`Machine`]), named after `label`, the process and a count, and
/// removed when dropped.
pub struct Place(pub PathBuf);

impl Place {
    pub fn new(on: &Path, label: &str) -> Place {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = on.join(format!(
            "{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("cannot make a place");
        Place(std::fs::canonicalize(path).expect("cannot resolve a place"))
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether this runs on a machine thrown away after its tests, as
/// Windows' runners do (`skwird_testkit::throwaway` there): a runner
/// GitHub hosts whose workflow sets `SKWIRD_TEST_MACHINE`
/// (`.github/workflows/macos.yml`). A test that changes the system (a
/// `LaunchAgent` taken by launchd) runs there alone, and is skipped
/// anywhere else: never on a developer's own system.
/// `SKWIRD_TEST_MACHINE` set anywhere else stops the test.
pub fn throwaway() -> bool {
    let asked = std::env::var_os("SKWIRD_TEST_MACHINE").is_some();
    // GitHub's own runners say so; a runner of someone's own says
    // `self-hosted`, and may be their machine.
    let hosted = std::env::var("RUNNER_ENVIRONMENT").is_ok_and(|v| v == "github-hosted");
    assert!(
        !asked || hosted,
        "SKWIRD_TEST_MACHINE is set, but this is not a runner GitHub hosts"
    );
    asked
}
