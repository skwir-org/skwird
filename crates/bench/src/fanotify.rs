//! Whether a system service could watch a whole filesystem here
//! (docs/DESIGN.md, "Privileges"): fanotify asked to report every name
//! made, removed or moved on the filesystem holding PATH, with the folder
//! it was in (`FAN_MARK_FILESYSTEM` and `FAN_REPORT_DFID_NAME`: Linux 5.9
//! and later, `CAP_SYS_ADMIN`).
//!
//! On btrfs every subvolume reports a filesystem id of its own, and
//! `fanotify_mark(2)` documents `EXDEV` for a path in a subvolume whose id is
//! not its superblock's. A mark refused so is tried again through the
//! filesystem's top-level subvolume, whose id is the superblock's: mounted
//! read-only and detached, attached to no folder so that no other process
//! sees it, and gone when the probe ends. What is asked is whether a mark
//! is accepted, which filesystem ids the events heard come with, and
//! whether the folder handles they hold open.
//!
//! A probe is a folder where the probe makes, renames and removes a file of
//! its own (`.skwird-probe-PID-N`) once the mark is set: its events are
//! told from everything else the filesystem does by their names, so the
//! answer for each probe is whether its own three changes were heard, and
//! whether the handle of the folder they were made in opens to it.
#![allow(unsafe_code)] // fanotify has no safe binding; each call says why it is sound.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, Stat};

use crate::said;

/// How a folder is opened to mark its filesystem, and to open the handles
/// its events hold (`open_by_handle_at` refuses an `O_PATH` descriptor).
const FOLDER: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// What was heard while listening.
#[derive(Default)]
struct Heard {
    /// Events by the filesystem id they came with, in the order first heard.
    per_fs: BTreeMap<usize, u64>,
    ids: Vec<u64>,
    /// Events read; fanotify merges changes to one name into one event.
    events: u64,
    made: u64,
    removed: u64,
    moved: u64,
    overflows: u64,
    /// The probes' own changes heard, by probe.
    probes: BTreeMap<usize, Seen>,
}

/// One probe's own changes heard, the filesystem ids they came with, and
/// the handle of the folder they were made in.
#[derive(Default)]
struct Seen {
    made: bool,
    moved: bool,
    removed: bool,
    ids: Vec<u64>,
    /// The folder's handle as the first event gave it: a `struct
    /// file_handle`, its length and type, then its bytes.
    handle: Option<Vec<u8>>,
}

/// A probe's changes made, and where they were made.
struct Probe {
    /// Why the changes could not be made, if they could not.
    failed: Option<String>,
    /// The folder's filesystem id, as `statvfs` gives it.
    fsid: Option<u64>,
    /// The folder itself, to tell whether a handle opens to it.
    folder: Option<Stat>,
    /// How its filesystem stands to the marked one.
    relation: String,
}

impl Probe {
    /// `marked` is the path the probe was asked about, on the marked
    /// filesystem; `own` the mark's filesystem id.
    fn make(index: usize, dir: &Path, prefix: &str, marked: &Path, own: u64) -> Probe {
        let fsid = rustix::fs::statvfs(dir).ok().map(|s| s.f_fsid);
        let (here, there) = (mounted_from(dir), mounted_from(marked));
        let relation = match (&here, &there, fsid) {
            (Some(a), Some(b), Some(id)) if a == b && id == own => {
                "the mark's filesystem id".into()
            }
            (Some(a), Some(b), Some(_)) if a == b => {
                "the same filesystem, another filesystem id (a subvolume of its own)".into()
            }
            (Some((kind, _)), _, _) => format!("another filesystem ({kind})"),
            _ => "a filesystem mountinfo does not say".into(),
        };
        let first = dir.join(format!("{prefix}{index}"));
        let second = dir.join(format!("{prefix}{index}-moved"));
        let failed = std::fs::write(&first, b"")
            .and_then(|()| std::fs::rename(&first, &second))
            .and_then(|()| std::fs::remove_file(&second))
            .err()
            .map(|e| e.to_string());
        if failed.is_some() {
            let _ = std::fs::remove_file(&first);
            let _ = std::fs::remove_file(&second);
        }
        Probe {
            failed,
            fsid,
            folder: rustix::fs::stat(dir).ok(),
            relation,
        }
    }
}

/// The filesystem a folder is on, as mountinfo names it: its type and
/// where it is mounted from (the device, the same for every subvolume of
/// one btrfs filesystem).
fn mounted_from(dir: &Path) -> Option<(String, PathBuf)> {
    let dir = std::fs::canonicalize(dir).ok()?;
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String, PathBuf)> = None;
    for line in info.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let (Some(point), Some(kind), Some(source)) = (
            before.split(' ').nth(4),
            after.split(' ').next(),
            after.split(' ').nth(1),
        ) else {
            continue;
        };
        let point = unescape(point);
        if !dir.starts_with(&point) {
            continue;
        }
        // The deepest mount holding the folder; of two at one place, the
        // later, which hides the other.
        let depth = point.components().count();
        if best.as_ref().is_none_or(|(d, ..)| depth >= *d) {
            best = Some((depth, kind.to_owned(), unescape(source)));
        }
    }
    best.map(|(_, kind, source)| (kind, source))
}

/// A mountinfo field as the path it stands for: `\040` is a space.
fn unescape(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
        match (bytes[i], octal) {
            (b'\\', Some(d)) => {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

pub fn probe(path: &Path, seconds: u64, probes: &[PathBuf]) -> Result<(), String> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    println!("kernel: {}", release.trim());
    let fs = rustix::fs::statfs(path).map_err(|e| format!("cannot statfs the path: {e}"))?;
    let kind = crate::walk::fs_name(fs.f_type as u64);
    println!("filesystem: {kind}");
    let group = group()?;
    let Some(marked) = watch(&group, path, &kind)? else {
        return Ok(());
    };
    let own = rustix::fs::fstatvfs(&marked.on)
        .map_err(|e| format!("cannot statvfs the mark: {e}"))?
        .f_fsid;
    let prefix = format!(".skwird-probe-{}-", std::process::id());
    let made: Vec<Probe> = probes
        .iter()
        .enumerate()
        .map(|(i, dir)| Probe::make(i, dir, &prefix, path, own))
        .collect();
    let seconds = if probes.is_empty() {
        seconds
    } else {
        seconds.max(1)
    };
    if seconds == 0 {
        return Ok(());
    }
    if probes.is_empty() {
        println!(
            "listening {seconds} s: make, rename and remove a file in each subvolume or \
             filesystem you care about, the path's own included"
        );
    } else {
        println!(
            "listening {seconds} s, a file made, renamed and removed in each of {} probes",
            probes.len()
        );
    }
    let heard = listen(&group, Duration::from_secs(seconds), &prefix)?;
    report(&heard, &made, own, &marked.on);
    Ok(())
}

/// A fanotify group that reports names with the folder they are in.
fn group() -> Result<OwnedFd, String> {
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
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Asks `group` to report every name made, removed or moved on the whole
/// filesystem holding the open folder `on`.
fn mark(group: &OwnedFd, on: &OwnedFd) -> std::io::Result<()> {
    let mask = libc::FAN_CREATE
        | libc::FAN_DELETE
        | libc::FAN_MOVED_FROM
        | libc::FAN_MOVED_TO
        | libc::FAN_ONDIR;
    // SAFETY: `group` is a fanotify descriptor and `on` an open folder; the
    // path is a NUL-terminated literal the call only reads.
    let marked = unsafe {
        libc::fanotify_mark(
            group.as_raw_fd(),
            libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
            mask,
            on.as_raw_fd(),
            c".".as_ptr(),
        )
    };
    if marked == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A mark set: the folder it was set through, whose filesystem id is the
/// mark's and through which the handles events hold are opened, and the
/// detached mount that folder is in, if one was made for it.
struct Marked {
    on: OwnedFd,
    _mount: Option<OwnedFd>,
}

/// Marks the whole filesystem holding the folder `path`: through it, or,
/// in a btrfs subvolume, through the filesystem's top-level subvolume.
/// `None` once said why no mark was accepted.
fn watch(group: &OwnedFd, path: &Path, kind: &str) -> Result<Option<Marked>, String> {
    let on = rustix::fs::open(path, FOLDER, Mode::empty())
        .map_err(|e| format!("cannot open the path as a folder: {e}"))?;
    let e = match mark(group, &on) {
        Ok(()) => {
            println!("mark: accepted, the whole filesystem holding the path is watched");
            return Ok(Some(Marked { on, _mount: None }));
        }
        Err(e) => e,
    };
    let why = match e.raw_os_error() {
        // Since Linux 5.13 anyone may make a group that reports names, but
        // only a holder of CAP_SYS_ADMIN may mark a whole filesystem.
        Some(libc::EPERM) => {
            return Err(
                "a mark of a whole filesystem needs root (CAP_SYS_ADMIN): run this with sudo"
                    .into(),
            );
        }
        Some(libc::EXDEV) if kind == "btrfs" => {
            println!(
                "mark through the path: refused, it is in a subvolume whose filesystem id is \
                 not its superblock's ({e})"
            );
            return Ok(through_top_level(group, path));
        }
        Some(libc::EXDEV) => format!("the path's filesystem id is not its superblock's ({e})"),
        Some(libc::ENODEV | libc::EOPNOTSUPP) => {
            format!("this filesystem cannot report file handles ({e})")
        }
        _ => format!("{e}"),
    };
    println!("mark: refused, {why}");
    Ok(None)
}

/// The mark set through the top-level subvolume of the btrfs filesystem
/// holding `path`, whose filesystem id is the superblock's, or `None` once
/// said why it was not.
fn through_top_level(group: &OwnedFd, path: &Path) -> Option<Marked> {
    let (mount, on) = top_level(path)
        .map_err(|why| println!("mark through the top-level subvolume: not tried, {why}"))
        .ok()?;
    match mark(group, &on) {
        Ok(()) => {
            println!(
                "mark through the top-level subvolume, mounted read-only and attached nowhere: \
                 accepted, the whole filesystem is watched"
            );
            Some(Marked {
                on,
                _mount: Some(mount),
            })
        }
        Err(e) => {
            println!("mark through the top-level subvolume: refused ({e})");
            None
        }
    }
}

/// The btrfs filesystem holding `path` mounted again at its top-level
/// subvolume (number 5), detached, and that mount's root opened.
///
/// The mount is attached to no folder, so no other process sees it, and it
/// goes when its descriptors close. It is read-only twice over: as a
/// mount, and as asked of btrfs, which leaves a writable filesystem as it
/// is when a subvolume is mounted read-only, but may make a read-only one
/// writable to mount one read-write.
fn top_level(path: &Path) -> Result<(OwnedFd, OwnedFd), String> {
    use rustix::mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, fsconfig_create, fsconfig_set_flag,
        fsconfig_set_string, fsmount, fsopen,
    };
    let failed = |step: &'static str| move |e: rustix::io::Errno| format!("{step} failed ({e})");
    let (_, source) = mounted_from(path).ok_or("mountinfo does not say the path's device")?;
    let fs = fsopen("btrfs", FsOpenFlags::FSOPEN_CLOEXEC).map_err(failed("fsopen"))?;
    fsconfig_set_string(&fs, "source", source.as_path()).map_err(failed("naming the device"))?;
    fsconfig_set_string(&fs, "subvolid", "5").map_err(failed("naming the subvolume"))?;
    fsconfig_set_flag(&fs, "ro").map_err(failed("asking read-only"))?;
    fsconfig_create(&fs).map_err(failed("mounting"))?;
    let mount = fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::MOUNT_ATTR_RDONLY
            | MountAttrFlags::MOUNT_ATTR_NOSUID
            | MountAttrFlags::MOUNT_ATTR_NODEV
            | MountAttrFlags::MOUNT_ATTR_NOEXEC,
    )
    .map_err(failed("fsmount"))?;
    let on = rustix::fs::openat(&mount, ".", FOLDER, Mode::empty())
        .map_err(failed("opening its top"))?;
    Ok((mount, on))
}

/// What was heard, then whether each probe's own changes were, and
/// whether its folder's handle opens through `on`.
fn report(heard: &Heard, made: &[Probe], own: u64, on: &OwnedFd) {
    println!(
        "heard: {} events, holding {} made, {} removed, {} moved; {} overflows",
        said::count(heard.events),
        said::count(heard.made),
        said::count(heard.removed),
        said::count(heard.moved),
        said::count(heard.overflows)
    );
    for (i, id) in heard.ids.iter().enumerate() {
        let letter = char::from(b'A' + (i % 26) as u8);
        let mine = if *id == own { " (the mark's)" } else { "" };
        println!(
            "  filesystem id {letter}{mine}: {} events",
            said::count(heard.per_fs.get(&i).copied().unwrap_or(0))
        );
    }
    if !heard.ids.contains(&own) {
        println!("  nothing heard with the mark's filesystem id");
    }
    for (i, probe) in made.iter().enumerate() {
        let said = match (&probe.failed, heard.probes.get(&i)) {
            (Some(why), _) => format!("could not make its file ({why})"),
            (None, None) => "none of its 3 changes heard".into(),
            (None, Some(seen)) => {
                let count = [seen.made, seen.moved, seen.removed]
                    .iter()
                    .filter(|&&b| b)
                    .count();
                let ids = if seen.ids.iter().all(|id| Some(*id) == probe.fsid) {
                    "with its own filesystem id"
                } else if seen.ids.iter().all(|id| *id == own) {
                    "with the mark's filesystem id"
                } else {
                    "with another filesystem id"
                };
                let handle = match (&seen.handle, &probe.folder) {
                    (None, _) => "no handle of its folder".into(),
                    (Some(_), None) => "its folder could not be stat-ed".into(),
                    (Some(handle), Some(folder)) => match open_handle(on, handle) {
                        Ok(st) if (st.st_dev, st.st_ino) == (folder.st_dev, folder.st_ino) => {
                            "its folder's handle opens to it".into()
                        }
                        Ok(_) => "its folder's handle opens to another folder".into(),
                        Err(e) => format!("its folder's handle does not open ({e})"),
                    },
                };
                format!("{count} of its 3 changes heard, {ids}; {handle}")
            }
        };
        println!("  probe {}: {}: {said}", i + 1, probe.relation);
    }
}

/// A filesystem id's two words as `statvfs` gives them: the first low.
fn fsid([low, high]: [i32; 2]) -> u64 {
    u64::from(low as u32) | (u64::from(high as u32) << 32)
}

/// The probe a name is one of: `PREFIX7` and `PREFIX7-moved` are probe 7's.
fn probe_of(name: &[u8], prefix: &str) -> Option<usize> {
    let rest = name.strip_prefix(prefix.as_bytes())?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()
}

fn listen(fd: &OwnedFd, how_long: Duration, prefix: &str) -> Result<Heard, String> {
    let meta_len = size_of::<libc::fanotify_event_metadata>();
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
        let read = &buf[..n as usize];
        let mut at = 0;
        while at + meta_len <= read.len() {
            // SAFETY: `meta_len` bytes from `at` are within what read wrote;
            // read_unaligned copies them whatever their alignment.
            let meta: libc::fanotify_event_metadata =
                unsafe { std::ptr::read_unaligned(read.as_ptr().add(at).cast()) };
            if meta.vers != libc::FANOTIFY_METADATA_VERSION {
                return Err(format!("fanotify metadata of version {}", meta.vers));
            }
            let end = at + meta.event_len as usize;
            if (meta.event_len as usize) < meta_len || end > read.len() {
                break;
            }
            event(&read[at..end], &meta, prefix, &mut heard);
            if meta.fd >= 0 {
                // SAFETY: an event's descriptor is the reader's to close.
                drop(unsafe { OwnedFd::from_raw_fd(meta.fd) });
            }
            at = end;
        }
    }
    Ok(heard)
}

/// One event, `bytes` from its metadata to its end: counted, its
/// filesystem id kept, and a probe's own change noted.
fn event(bytes: &[u8], meta: &libc::fanotify_event_metadata, prefix: &str, heard: &mut Heard) {
    let header_len = size_of::<libc::fanotify_event_info_header>();
    let fid_len = size_of::<libc::fanotify_event_info_fid>();
    let moved = meta.mask & (libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO) != 0;
    heard.events += 1;
    heard.overflows += u64::from(meta.mask & libc::FAN_Q_OVERFLOW != 0);
    heard.made += u64::from(meta.mask & libc::FAN_CREATE != 0);
    heard.removed += u64::from(meta.mask & libc::FAN_DELETE != 0);
    heard.moved += u64::from(moved);
    let mut info = meta.metadata_len as usize;
    while info + header_len <= bytes.len() {
        // SAFETY: `header_len` bytes from `info` are within the event.
        let header: libc::fanotify_event_info_header =
            unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(info).cast()) };
        let record_end = (info + header.len as usize).min(bytes.len());
        if header.len == 0 {
            break;
        }
        let names_a_file = matches!(
            header.info_type,
            libc::FAN_EVENT_INFO_TYPE_FID
                | libc::FAN_EVENT_INFO_TYPE_DFID_NAME
                | libc::FAN_EVENT_INFO_TYPE_DFID
        );
        if !names_a_file || info + fid_len > record_end {
            info += header.len as usize;
            continue;
        }
        // SAFETY: `fid_len` bytes from `info` are within the record.
        let fid: libc::fanotify_event_info_fid =
            unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(info).cast()) };
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
        let name = (header.info_type == libc::FAN_EVENT_INFO_TYPE_DFID_NAME)
            .then(|| name_after_handle(&bytes[info + fid_len..record_end]))
            .flatten();
        if let Some(p) = name.and_then(|name| probe_of(name, prefix)) {
            let seen = heard.probes.entry(p).or_default();
            seen.made |= meta.mask & libc::FAN_CREATE != 0;
            seen.moved |= moved;
            seen.removed |= meta.mask & libc::FAN_DELETE != 0;
            if !seen.ids.contains(&id) {
                seen.ids.push(id);
            }
            if seen.handle.is_none() {
                seen.handle = handle_of(&bytes[info + fid_len..record_end]).map(<[u8]>::to_vec);
            }
        }
        break;
    }
}

/// The folder's handle a record holds, as a `struct file_handle`: its
/// length (4 bytes), its type (4), then its bytes.
fn handle_of(record: &[u8]) -> Option<&[u8]> {
    let length = u32::from_ne_bytes(record.get(..4)?.try_into().ok()?) as usize;
    record.get(..8 + length)
}

/// The name a record holds after its folder's handle, ending with a NUL.
fn name_after_handle(record: &[u8]) -> Option<&[u8]> {
    let name = record.get(handle_of(record)?.len()..)?;
    Some(&name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())])
}

/// What a folder's handle opens to, through `on`, a folder on its
/// filesystem.
fn open_handle(on: &OwnedFd, handle: &[u8]) -> std::io::Result<Stat> {
    // Copied into words, so that it is aligned as a `struct file_handle`.
    let mut words: Vec<u32> = handle
        .chunks(4)
        .map(|chunk| {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            u32::from_ne_bytes(word)
        })
        .collect();
    // SAFETY: `words` holds a whole `struct file_handle` as the event gave
    // it (its length, its type, then that many bytes), aligned for one, and
    // the kernel only reads it.
    let raw = unsafe {
        libc::open_by_handle_at(
            on.as_raw_fd(),
            words.as_mut_ptr().cast(),
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` was just returned by open_by_handle_at and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(rustix::fs::fstat(&fd)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mountinfo's escapes read back as the path they stand for.
    #[test]
    fn a_mount_point_reads_as_its_path() {
        assert_eq!(
            unescape("/run/media/a\\040b"),
            PathBuf::from("/run/media/a b")
        );
        assert_eq!(unescape("/plain"), PathBuf::from("/plain"));
        assert_eq!(unescape("/odd\\x"), PathBuf::from("/odd\\x"));
    }

    /// The name a record holds is read after its folder's handle.
    #[test]
    fn a_name_is_read_after_its_folders_handle() {
        let mut record = Vec::new();
        record.extend_from_slice(&3u32.to_ne_bytes());
        record.extend_from_slice(&1i32.to_ne_bytes());
        record.extend_from_slice(&[9, 9, 9]);
        record.extend_from_slice(b"a b.txt\0\0\0");
        assert_eq!(handle_of(&record), Some(&record[..11]));
        assert_eq!(name_after_handle(&record), Some(&b"a b.txt"[..]));
        assert_eq!(handle_of(&record[..10]), None, "a handle cut short");
        assert_eq!(name_after_handle(&record[..6]), None, "cut short");
    }

    /// A probe's names are told from any other.
    #[test]
    fn a_probe_is_known_by_its_names() {
        let prefix = ".skwird-probe-42-";
        assert_eq!(probe_of(b".skwird-probe-42-3", prefix), Some(3));
        assert_eq!(probe_of(b".skwird-probe-42-12-moved", prefix), Some(12));
        assert_eq!(probe_of(b".skwird-probe-41-3", prefix), None);
        assert_eq!(probe_of(b"notes.txt", prefix), None);
    }
}
