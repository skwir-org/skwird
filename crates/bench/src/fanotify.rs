//! Whether a system service could watch a whole filesystem here
//! (docs/DESIGN.md, "Privileges"): fanotify asked to report every name
//! made, removed or moved on the filesystem holding PATH, with the folder
//! it was in (`FAN_MARK_FILESYSTEM` and `FAN_REPORT_DFID_NAME`: Linux 5.9
//! and later, `CAP_SYS_ADMIN`).
//!
//! On btrfs every subvolume reports a filesystem id of its own, and
//! `fanotify_mark(2)` documents `EXDEV` for a path in a subvolume whose id is
//! not its superblock's. What is asked here is whether this kernel refuses
//! the mark, and which filesystem ids the events heard come with.
#![allow(unsafe_code)] // fanotify has no safe binding; each call says why it is sound.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::said;

/// What was heard while listening.
#[derive(Default)]
struct Heard {
    /// Events by the filesystem id they came with, in the order first heard.
    per_fs: BTreeMap<usize, u64>,
    ids: Vec<u64>,
    made: u64,
    removed: u64,
    moved: u64,
    overflows: u64,
}

pub fn probe(path: &Path, seconds: u64) -> Result<(), String> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    println!("kernel: {}", release.trim());
    let fs = rustix::fs::statfs(path).map_err(|e| format!("cannot statfs the path: {e}"))?;
    let own = rustix::fs::statvfs(path)
        .map_err(|e| format!("cannot statvfs the path: {e}"))?
        .f_fsid;
    println!("filesystem: {}", crate::walk::fs_name(fs.f_type as u64));
    // SAFETY: fanotify_init takes two words of flags and returns a new
    // descriptor or -1; no memory is shared with the kernel.
    let raw = unsafe {
        libc::fanotify_init(
            libc::FAN_CLASS_NOTIF
                | libc::FAN_CLOEXEC
                | libc::FAN_NONBLOCK
                | libc::FAN_REPORT_DFID_NAME,
            (libc::O_RDONLY | libc::O_LARGEFILE) as u32,
        )
    };
    if raw < 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EPERM) => {
                "fanotify needs root here (CAP_SYS_ADMIN): run this with sudo".into()
            }
            Some(libc::EINVAL) => format!("this kernel cannot report names through fanotify ({e})"),
            _ => format!("fanotify_init: {e}"),
        });
    }
    // SAFETY: `raw` was just returned by fanotify_init and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| "a path with a NUL")?;
    let mask = libc::FAN_CREATE
        | libc::FAN_DELETE
        | libc::FAN_MOVED_FROM
        | libc::FAN_MOVED_TO
        | libc::FAN_ONDIR;
    // SAFETY: `fd` is a fanotify descriptor, `c_path` a NUL-terminated path
    // that outlives the call.
    let marked = unsafe {
        libc::fanotify_mark(
            fd.as_raw_fd(),
            libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
            mask,
            libc::AT_FDCWD,
            c_path.as_ptr(),
        )
    };
    if marked < 0 {
        let e = std::io::Error::last_os_error();
        // Since Linux 5.13 anyone may make a group that reports names, but
        // only a holder of CAP_SYS_ADMIN may mark a whole filesystem.
        if e.raw_os_error() == Some(libc::EPERM) {
            return Err(
                "a mark of a whole filesystem needs root (CAP_SYS_ADMIN): run this with sudo"
                    .into(),
            );
        }
        println!(
            "mark: refused, {}",
            match e.raw_os_error() {
                Some(libc::EXDEV) => format!(
                    "the path is in a subvolume whose filesystem id is not its superblock's ({e}): \
                     mark the filesystem's top, or each subvolume needs another way"
                ),
                Some(libc::ENODEV | libc::EOPNOTSUPP) =>
                    format!("this filesystem cannot report file handles ({e})"),
                _ => format!("{e}"),
            }
        );
        return Ok(());
    }
    println!("mark: accepted, the whole filesystem holding the path is watched");
    if seconds == 0 {
        return Ok(());
    }
    println!(
        "listening {seconds} s: make, rename and remove a file in each subvolume or \
         filesystem you care about, the path's own included"
    );
    let heard = listen(&fd, Duration::from_secs(seconds))?;
    println!(
        "heard: {} made, {} removed, {} moved, {} overflows",
        said::count(heard.made),
        said::count(heard.removed),
        said::count(heard.moved),
        said::count(heard.overflows)
    );
    for (i, id) in heard.ids.iter().enumerate() {
        let letter = char::from(b'A' + (i % 26) as u8);
        let mine = if *id == own { " (the path's own)" } else { "" };
        println!(
            "  filesystem id {letter}{mine}: {} events",
            said::count(heard.per_fs.get(&i).copied().unwrap_or(0))
        );
    }
    if !heard.ids.contains(&own) {
        println!("  nothing heard with the path's own filesystem id");
    }
    Ok(())
}

/// A filesystem id's two words as `statvfs` gives them: the first low.
fn fsid([low, high]: [i32; 2]) -> u64 {
    u64::from(low as u32) | (u64::from(high as u32) << 32)
}

fn listen(fd: &OwnedFd, how_long: Duration) -> Result<Heard, String> {
    let meta_len = size_of::<libc::fanotify_event_metadata>();
    let header_len = size_of::<libc::fanotify_event_info_header>();
    let fid_len = size_of::<libc::fanotify_event_info_fid>();
    let mut heard = Heard::default();
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + how_long;
    while Instant::now() < deadline {
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes, the most
        // read writes.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            return Err(format!("reading events: {e}"));
        }
        let n = n as usize;
        let mut at = 0;
        while at + meta_len <= n {
            // SAFETY: `meta_len` bytes from `at` are within what read wrote;
            // read_unaligned copies them whatever their alignment.
            let meta: libc::fanotify_event_metadata =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at).cast()) };
            if meta.vers != libc::FANOTIFY_METADATA_VERSION {
                return Err(format!("fanotify metadata of version {}", meta.vers));
            }
            let end = at + meta.event_len as usize;
            if (meta.event_len as usize) < meta_len || end > n {
                break;
            }
            if meta.mask & libc::FAN_Q_OVERFLOW != 0 {
                heard.overflows += 1;
            }
            if meta.mask & libc::FAN_CREATE != 0 {
                heard.made += 1;
            }
            if meta.mask & libc::FAN_DELETE != 0 {
                heard.removed += 1;
            }
            if meta.mask & (libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO) != 0 {
                heard.moved += 1;
            }
            let mut info = at + meta.metadata_len as usize;
            while info + header_len <= end {
                // SAFETY: within the event, as above.
                let header: libc::fanotify_event_info_header =
                    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(info).cast()) };
                if header.len == 0 {
                    break;
                }
                let names_a_file = matches!(
                    header.info_type,
                    libc::FAN_EVENT_INFO_TYPE_FID
                        | libc::FAN_EVENT_INFO_TYPE_DFID_NAME
                        | libc::FAN_EVENT_INFO_TYPE_DFID
                );
                if names_a_file && info + fid_len <= end {
                    // SAFETY: within the event, as above.
                    let fid: libc::fanotify_event_info_fid =
                        unsafe { std::ptr::read_unaligned(buf.as_ptr().add(info).cast()) };
                    let id = fsid(fid.fsid.val);
                    let i = heard
                        .ids
                        .iter()
                        .position(|known| *known == id)
                        .unwrap_or_else(|| {
                            heard.ids.push(id);
                            heard.ids.len() - 1
                        });
                    *heard.per_fs.entry(i).or_default() += 1;
                    break;
                }
                info += header.len as usize;
            }
            if meta.fd >= 0 {
                // SAFETY: an event's descriptor is the reader's to close.
                drop(unsafe { OwnedFd::from_raw_fd(meta.fd) });
            }
            at = end;
        }
    }
    Ok(heard)
}
