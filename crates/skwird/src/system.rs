//! What the system service covers (`docs/DESIGN.md`, decision 1, the
//! user's choice of 2026-11-22): every local filesystem that stores files,
//! except the Nix store. Never the kernel's own filesystems or memory
//! ones, network mounts, nor removable volumes (open question 8); and in
//! every home, the default exclusions (decision 5).

#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use skwird_crawl::mounts::{Mount, Table};

/// The filesystems that store files on a disk of the machine's own.
/// Anything else is the kernel's (proc, sysfs, cgroup), memory (tmpfs,
/// ramfs), another filesystem's names again (overlay, a FUSE mirror), an
/// image's (squashfs, erofs, iso9660), or a network's.
const STORING: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "bcachefs", "zfs", "jfs", "reiserfs", "nilfs2",
    "vfat", "exfat", "ntfs3", "ntfs", "hfsplus",
];

/// The Nix store: millions of read-only entries, changed by nix's builds
/// alone, whose names nix answers for itself.
pub const NIX_STORE: &str = "/nix/store";

/// The roots of the system service: the point of each mount of a
/// filesystem that stores files, not removable, not reaching a network,
/// shown once (a bind mount of what another mount shows already is left
/// out), and not the Nix store or within it.
pub fn roots(table: &Table, removable: impl Fn(&Mount) -> bool) -> Vec<PathBuf> {
    let mounts = table.mounts();
    let mut kept: Vec<&Mount> = Vec::new();
    for (i, m) in mounts.iter().enumerate() {
        // A later mount at the same point hides this one.
        let hidden = mounts[i + 1..].iter().any(|later| later.point == m.point);
        if hidden
            || !STORING.contains(&m.fstype.as_str())
            || m.network()
            || m.point.starts_with(NIX_STORE)
            || removable(m)
        {
            continue;
        }
        // What another mount of the same filesystem shows already: a bind
        // mount of it, or of a folder within it.
        if kept
            .iter()
            .any(|k| k.dev == m.dev && m.root.starts_with(&k.root))
        {
            continue;
        }
        kept.retain(|k| !(k.dev == m.dev && k.root.starts_with(&m.root)));
        kept.push(m);
    }
    let mut roots: Vec<PathBuf> = kept.iter().map(|m| m.point.clone()).collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Whether a mount's filesystem is on a removable volume: a disk the
/// kernel says is removable, or one on a USB bus.
#[cfg(target_os = "linux")]
pub fn removable(mount: &Mount) -> bool {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    // A btrfs's device in the table is its own anonymous one: the disk is
    // the source's.
    let (major, minor) = if mount.dev.0 == 0 {
        let Ok(meta) = std::fs::metadata(&mount.source) else {
            return false;
        };
        if !meta.file_type().is_block_device() {
            return false;
        }
        let rdev = meta.rdev();
        (rustix::fs::major(rdev), rustix::fs::minor(rdev))
    } else {
        mount.dev
    };
    let Ok(device) = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")) else {
        return false;
    };
    let says = |dir: &std::path::Path| {
        std::fs::read_to_string(dir.join("removable")).is_ok_and(|s| s.trim() == "1")
    };
    device.to_string_lossy().contains("/usb") || says(&device) || device.parent().is_some_and(says)
}

#[cfg(not(target_os = "linux"))]
pub fn removable(_: &Mount) -> bool {
    false
}

/// The homes the default exclusions hold in: each user database entry's
/// home that is a folder (`/etc/passwd`), every folder in `/home`, and
/// root's. A home only another source of the user database names (LDAP,
/// systemd-homed) under none of these gets what its client registers.
pub fn homes() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = std::fs::read_to_string("/etc/passwd")
        .map(|text| passwd_homes(&text))
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir("/home") {
        homes.extend(entries.flatten().map(|e| e.path()));
    }
    homes.push(PathBuf::from("/root"));
    homes.retain(|h| h.as_os_str() != "/" && h.is_absolute());
    homes.sort();
    homes.dedup();
    homes
}

/// The homes a user database's text names, one entry a line
/// (`name:password:uid:gid:gecos:home:shell`).
fn passwd_homes(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| line.split(':').nth(5))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The users the system service keeps exclusions for (DESIGN 5): each
/// user database entry's id and home, and each folder in `/home` by its
/// owner, for a home only another source of the user database names.
#[cfg(target_os = "linux")]
pub fn users() -> Vec<(u32, PathBuf)> {
    use std::os::unix::fs::MetadataExt;
    let mut users: Vec<(u32, PathBuf)> = std::fs::read_to_string("/etc/passwd")
        .map(|text| passwd_users(&text))
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir("/home") {
        for e in entries.flatten() {
            let path = e.path();
            if let Ok(meta) = std::fs::symlink_metadata(&path)
                && meta.is_dir()
                && !users.iter().any(|(_, h)| *h == path)
            {
                users.push((meta.uid(), path));
            }
        }
    }
    users.retain(|(_, home)| home.as_os_str() != "/" && home.is_absolute());
    users
}

/// Each entry's id and home, of a user database's text.
#[cfg(target_os = "linux")]
fn passwd_users(text: &str) -> Vec<(u32, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let uid = fields.get(2)?.parse().ok()?;
            let home = fields.get(5).filter(|h| !h.is_empty())?;
            Some((uid, PathBuf::from(home)))
        })
        .collect()
}

/// A file of a user's own, at `rel` in their home, read for the system
/// service: opened through the home's handle, following no link and
/// staying beneath it, and taken only if it is a file the user or root
/// owns and no one else may write, as sshd reads a user's keys. At most a
/// mebibyte.
#[cfg(target_os = "linux")]
pub fn read_own(home: &Path, uid: u32, rel: &str) -> Option<Vec<u8>> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    use std::io::Read;
    let home_fd = rustix::fs::open(
        home,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let fd = rustix::fs::openat2(
        &home_fd,
        rel,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .ok()?;
    let st = rustix::fs::fstat(&fd).ok()?;
    let file = rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile;
    if !file || (st.st_uid != uid && st.st_uid != 0) || st.st_mode & 0o022 != 0 {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(1 << 20)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// A folder a user's engine's clients registered, as the file keeps it.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct Registered {
    path: skwird_protocol::WirePath,
}

/// What a user keeps out of the index, read from their home: their
/// exclusions (`~/.config/skwird/exclude`, one path a line, `~/` their
/// home, `#` a comment) and what their engine's clients registered
/// (`~/.local/state/skwird/registered`), at the default places, since the
/// service knows no user's environment.
#[cfg(target_os = "linux")]
pub fn own_exclusions(home: &Path, uid: u32) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(text) = read_own(home, uid, ".config/skwird/exclude") {
        let text = String::from_utf8_lossy(&text);
        paths.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| match l.strip_prefix("~/") {
                    Some(rest) => home.join(rest),
                    None => PathBuf::from(l),
                }),
        );
    }
    if let Some(bytes) = read_own(home, uid, ".local/state/skwird/registered")
        && let Ok(all) = serde_json::from_slice::<Vec<Registered>>(&bytes)
    {
        paths.extend(all.into_iter().map(|r| r.path.to_path()));
    }
    paths
}

/// Whether user `uid`, of home `home`, owns the folder an exclusion of
/// theirs resolved to: at `resolved`, of identity `id` if one is there.
/// Root owns every folder. A folder there is theirs if it is still that
/// folder and they own it; a path where none is there yet, if it is in
/// their home and they own that.
#[cfg(target_os = "linux")]
pub fn owned(uid: u32, home: &Path, resolved: &Path, id: Option<skwird_crawl::FileId>) -> bool {
    use std::os::unix::fs::MetadataExt;
    if uid == 0 {
        return true;
    }
    match id {
        Some(id) => std::fs::symlink_metadata(resolved)
            .is_ok_and(|m| m.dev() == id.dev && m.ino() == id.ino && m.uid() == uid),
        None => {
            resolved.starts_with(home)
                && std::fs::symlink_metadata(home).is_ok_and(|m| m.uid() == uid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(lines: &[&str]) -> Table {
        Table::parse(lines.join("\n").as_bytes())
    }

    fn paths(p: &[&str]) -> Vec<PathBuf> {
        p.iter().map(PathBuf::from).collect()
    }

    /// The filesystems that store files are covered; the kernel's, memory
    /// ones, images, unions and networks are not, nor the Nix store.
    #[test]
    fn the_roots_are_the_filesystems_that_store_files() {
        let t = table(&[
            "22 1 0:26 / /proc rw - proc proc rw",
            "23 1 0:21 / /sys rw - sysfs sysfs rw",
            "25 1 0:30 /@ / rw - btrfs /dev/mapper/root rw",
            "26 25 0:30 /@home /home rw - btrfs /dev/mapper/root rw",
            "27 25 259:1 / /boot rw - vfat /dev/nvme0n1p1 rw",
            "28 25 0:31 / /run rw - tmpfs tmpfs rw",
            "29 25 0:30 /@/nix/store /nix/store ro - btrfs /dev/mapper/root rw",
            "30 25 0:40 / /srv/share rw - nfs4 server:/share rw",
            "31 25 7:0 / /snap/app/1 ro - squashfs /dev/loop0 ro",
            "32 25 0:41 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw",
            "33 25 259:2 / /data rw - ext4 /dev/nvme0n1p2 rw",
        ]);
        assert_eq!(
            roots(&t, |_| false),
            paths(&["/", "/boot", "/data", "/home"])
        );
    }

    /// A bind mount shows what another mount shows already: it is left
    /// out, whichever comes first; a hidden mount is left out too.
    #[test]
    fn a_filesystem_shown_twice_is_a_root_once() {
        let t = table(&[
            "25 1 8:1 / / rw - ext4 /dev/sda1 rw",
            "26 25 8:2 /srv /srv rw - ext4 /dev/sda2 rw",
            "27 25 8:2 /srv/www /var/www rw - ext4 /dev/sda2 rw",
            "28 25 8:3 / /mnt/old rw - ext4 /dev/sda3 rw",
            "29 25 8:4 / /mnt/old rw - ext4 /dev/sda4 rw",
        ]);
        assert_eq!(roots(&t, |_| false), paths(&["/", "/mnt/old", "/srv"]));
        // Of the two at /mnt/old, the later, which hides the earlier.
        let t2 = table(&[
            "29 1 8:4 /www /var/www rw - ext4 /dev/sda4 rw",
            "30 1 8:4 / /srv rw - ext4 /dev/sda4 rw",
        ]);
        assert_eq!(roots(&t2, |_| false), paths(&["/srv"]));
    }

    /// A removable volume is not covered.
    #[test]
    fn a_removable_volume_is_not_a_root() {
        let t = table(&[
            "25 1 8:1 / / rw - ext4 /dev/sda1 rw",
            "26 25 8:17 / /run/media/someone/stick rw - vfat /dev/sdb1 rw",
        ]);
        assert_eq!(
            roots(&t, |m| m.point.starts_with("/run/media")),
            paths(&["/"])
        );
    }

    #[test]
    fn homes_are_read_from_the_user_database() {
        let text = "root:x:0:0::/root:/bin/sh\nsomeone:x:1000:100:Some One:/home/someone:/bin/sh\nbroken\nnobody:x:65534:65534::/var/empty:/bin/false\n";
        assert_eq!(
            passwd_homes(text),
            paths(&["/root", "/home/someone", "/var/empty"])
        );
    }
}
