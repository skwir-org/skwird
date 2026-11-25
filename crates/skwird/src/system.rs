//! What the system service covers (`docs/DESIGN.md`, decision 1, the
//! user's choice of 2026-11-22): every local filesystem that stores files,
//! except the Nix store. Never the kernel's own filesystems or memory
//! ones, network mounts, nor removable volumes (open question 8); and in
//! every home, the default exclusions (decision 5). On macOS, `/` and the
//! Data volume's folders where the system links them (firmlinks): every
//! path as users name it (phase 5, step 7).

use std::path::{Path, PathBuf};

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

/// The system service's roots as `table` finds them (DESIGN 1): on
/// Linux, the mounts that store files ([`roots`]); on macOS, `/` and the
/// Data volume's folders where the system links them ([`firmlinked`]).
pub fn found(table: &Table) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    return firmlinked(table);
    #[cfg(not(target_os = "macos"))]
    roots(table, removable)
}

/// The roots of the system service on macOS (DESIGN 1, phase 5, step
/// 7): `/`, the system's volume, and each folder of the Data volume at
/// the place in `/` the system links it to (a firmlink, which the table
/// shows as a mount of its folder: `/Users`, `/Applications`), so that
/// every path is as users name it. The Data volume's own mount, and the
/// system's other volumes below `/System/Volumes`, are left out: the
/// first would index every file twice. So is every other volume, below
/// `/Volumes`: a disk inside the machine and one plugged in look alike
/// there, and either is a root when named.
pub fn firmlinked(table: &Table) -> Vec<PathBuf> {
    let system = table
        .mounts()
        .iter()
        .rev()
        .find(|m| m.point == Path::new("/"))
        .filter(|m| !m.network());
    let mut roots: Vec<PathBuf> = system
        .map(|m| m.point.clone())
        .into_iter()
        .chain(
            table
                .firmlinks()
                .filter(|m| !m.network())
                .map(|m| m.point.clone()),
        )
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Whether a mount's filesystem is on a removable volume, as a user's
/// engine tells one it may keep (`skwird_crawl::removable`): one
/// definition, so that a volume the system service leaves out is one a
/// user's engine sees. Its own copy missed an encrypted volume's chain of
/// devices and the SD card's bus, and indexed both as system roots.
#[cfg(target_os = "linux")]
pub fn removable(mount: &Mount) -> bool {
    skwird_crawl::removable::is_removable(mount)
}

#[cfg(not(target_os = "linux"))]
pub fn removable(_: &Mount) -> bool {
    false
}

/// Where users' homes are: `/home` on Linux, `/Users` on macOS.
#[cfg(target_os = "linux")]
pub const HOMES: &str = "/home";
#[cfg(target_os = "macos")]
pub const HOMES: &str = "/Users";

/// The users the system service keeps exclusions for (DESIGN 5): each
/// entry's id and home of the user database's file, and each folder in
/// [`HOMES`] by its owner, for a home only another source of the user
/// database names. macOS' file names the system's users alone, root
/// among them: its people are in its directory, each with a home in
/// `/Users`. The directory is not asked: on a Mac joined to a network
/// one, listing its users would ask the network's (rule 6).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn users() -> Vec<(u32, PathBuf)> {
    use std::os::unix::fs::MetadataExt;
    let mut users: Vec<(u32, PathBuf)> = std::fs::read_to_string("/etc/passwd")
        .map(|text| passwd_users(&text))
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir(HOMES) {
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// What came of reading a user's file.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub enum Own {
    /// Not there.
    Absent,
    /// There, but not taken: not a file, not theirs or root's (on Windows,
    /// `LocalSystem`'s or the administrators'), others may write it, or a
    /// link was in its way.
    Refused,
    Read(Vec<u8>),
}

/// A file of a user's own, at `rel` in their home, read for the system
/// service: opened through the home's handle, following no link and
/// staying beneath it, and taken only if it is a file the user or root
/// owns that no one else may write, as sshd reads a user's keys: its
/// group may write it only if that group is the user's own private one
/// (umask 002, with a group of each user's own). At most a mebibyte.
#[cfg(target_os = "linux")]
pub fn read_own(home: &Path, uid: u32, rel: &str) -> Own {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    let Ok(home_fd) = rustix::fs::open(
        home,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Own::Absent;
    };
    let fd = match rustix::fs::openat2(
        &home_fd,
        rel,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Own::Absent,
        Err(_) => return Own::Refused,
    };
    take_own(fd, uid)
}

/// A file of a user's own, at `rel` in their home, read for the system
/// service (macOS): opened through the home's handle by a path that may
/// hold no link at all (`O_NOFOLLOW_ANY`) and no `..` (`rel` is one of
/// skwird's), and taken as on Linux. macOS gives no user a group of their
/// own: a file their group may write is refused.
#[cfg(target_os = "macos")]
pub fn read_own(home: &Path, uid: u32, rel: &str) -> Own {
    use rustix::fs::{Mode, OFlags};
    debug_assert!(!rel.split('/').any(|n| n == ".."));
    let Ok(home_fd) = rustix::fs::open(
        home,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Own::Absent;
    };
    let fd = match rustix::fs::openat(
        &home_fd,
        rel,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW_ANY | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Own::Absent,
        Err(_) => return Own::Refused,
    };
    take_own(fd, uid)
}

/// The file `fd` opened, taken if it is a file user `uid` or root owns
/// that no one else may write: at most a mebibyte of it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn take_own(fd: rustix::fd::OwnedFd, uid: u32) -> Own {
    use std::io::Read;
    let Ok(st) = rustix::fs::fstat(&fd) else {
        return Own::Refused;
    };
    let file = rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile;
    let others_write = st.st_mode & 0o002 != 0;
    let group_writes = st.st_mode & 0o020 != 0 && !private_group(uid, st.st_gid);
    if !file || (st.st_uid != uid && st.st_uid != 0) || others_write || group_writes {
        return Own::Refused;
    }
    let mut bytes = Vec::new();
    match std::fs::File::from(fd)
        .take(1 << 20)
        .read_to_end(&mut bytes)
    {
        Ok(_) => Own::Read(bytes),
        Err(_) => Own::Refused,
    }
}

/// No group is a user's own on macOS: every user's primary group is the
/// same (`staff`).
#[cfg(target_os = "macos")]
fn private_group(_: u32, _: u32) -> bool {
    false
}

/// Whether group `gid` is user `uid`'s own private group: their primary
/// group, no one else's primary group, and naming no other member.
#[cfg(target_os = "linux")]
fn private_group(uid: u32, gid: u32) -> bool {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    is_private_group(&passwd, &group, uid, gid)
}

#[cfg(target_os = "linux")]
fn is_private_group(passwd: &str, group: &str, uid: u32, gid: u32) -> bool {
    let entries: Vec<(&str, u32, u32)> = passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((
                *f.first()?,
                f.get(2)?.parse().ok()?,
                f.get(3)?.parse().ok()?,
            ))
        })
        .collect();
    let Some(&(name, _, primary)) = entries.iter().find(|(_, u, _)| *u == uid) else {
        return false;
    };
    let alone = entries.iter().all(|&(_, u, g)| g != gid || u == uid);
    let members_alone = group.lines().all(|l| {
        let f: Vec<&str> = l.split(':').collect();
        f.get(2).and_then(|g| g.parse::<u32>().ok()) != Some(gid)
            || f.get(3)
                .is_none_or(|m| m.split(',').all(|m| m.is_empty() || m == name))
    });
    primary == gid && alone && members_alone
}

/// Where a user's engine keeps their exclusions and what its clients
/// registered, below their home, at the default places.
// On macOS too: a user's engine keeps its files at the XDG places there.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const OWN_FILES: (&str, &str) = (".config/skwird/exclude", ".local/state/skwird/registered");
#[cfg(windows)]
const OWN_FILES: (&str, &str) = (
    r"AppData\Local\skwird\config\exclude",
    r"AppData\Local\skwird\state\registered",
);

/// A folder a user's engine's clients registered, as the file keeps it.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[derive(serde::Deserialize)]
struct Registered {
    path: skwird_protocol::WirePath,
}

/// What a user keeps out of the index, read from their home: their
/// exclusions (`~/.config/skwird/exclude`, one path a line, `~/` their
/// home, `#` a comment) and what their engine's clients registered
/// (`~/.local/state/skwird/registered`), at the default places, since the
/// service knows no user's environment; and how many of the two files
/// were there but not taken ([`read_own`]). On Windows, in their profile's local
/// application data (`AppData\Local\skwird\config\exclude`,
/// `AppData\Local\skwird\state\registered`), where their engine keeps them.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn own_exclusions(home: &Path, uid: u32) -> (Vec<PathBuf>, usize) {
    let mut paths = Vec::new();
    let mut refused = 0;
    let mut read = |rel: &str| match read_own(home, uid, rel) {
        Own::Read(bytes) => Some(bytes),
        Own::Refused => {
            refused += 1;
            None
        }
        Own::Absent => None,
    };
    let (exclude, registered) = OWN_FILES;
    let exclude = read(exclude);
    let registered = read(registered);
    if let Some(text) = exclude {
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
    if let Some(bytes) = registered
        && let Ok(all) = serde_json::from_slice::<Vec<Registered>>(&bytes)
    {
        paths.extend(all.into_iter().map(|r| r.path.to_path()));
    }
    (paths, refused)
}

/// Whether user `uid`, of home `home`, owns the folder an exclusion of
/// theirs resolved to: at `resolved`, of identity `id` if one is there.
/// Root owns every folder. A folder there is theirs if it is still that
/// folder and they own it; a path where none is there yet, if it is in
/// their home and they own that.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn owned(uid: u32, home: &Path, resolved: &Path, id: Option<skwird_crawl::FileId>) -> bool {
    use std::os::unix::fs::MetadataExt;
    if uid == 0 {
        return true;
    }
    match id {
        Some(id) => std::fs::symlink_metadata(resolved)
            .is_ok_and(|m| dev_of(&m) == id.dev && m.ino() == id.ino && m.uid() == uid),
        None => {
            resolved.starts_with(home)
                && std::fs::symlink_metadata(home).is_ok_and(|m| m.uid() == uid)
        }
    }
}

/// A file's device, as the crawl keeps it: on macOS, its 32 bits, which
/// std sign-extends.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn dev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if cfg!(target_os = "macos") {
        meta.dev() & 0xFFFF_FFFF
    } else {
        meta.dev()
    }
}

// The system service on Windows (phase 4, step 7): its roots the
// machine's volumes, its users told by their SIDs and numbered, their
// homes their profiles, their own files read and their folders owned as
// their security descriptors say.
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    keep_numbers, number_of, numbering, owned, read_own, roots as volume_roots, sid_of, users,
};

/// No system service runs on this system (rule 7): no user's exclusions.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn users() -> Vec<(u32, PathBuf)> {
    Vec::new()
}

/// No user's file of exclusions is read on this system.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn own_exclusions(_: &Path, _: u32) -> (Vec<PathBuf>, usize) {
    (Vec::new(), 0)
}

/// No folder is any user's own on this system: none is registered for one.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn owned(_: u32, _: &Path, _: &Path, _: Option<skwird_crawl::FileId>) -> bool {
    false
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

    /// macOS' system service takes `/` and each firmlink, by the place
    /// users name it at, and leaves out the Data volume's own mount, the
    /// system's other volumes, a volume below `/Volumes` and a network's.
    #[cfg(unix)]
    #[test]
    fn on_macos_the_roots_are_the_system_volume_and_the_firmlinks() {
        let mount = |point: &str, dev: (u32, u32), fstype: &str| Mount {
            id: u64::from(dev.1),
            parent: 0,
            dev,
            root: PathBuf::from("/"),
            point: PathBuf::from(point),
            fstype: fstype.into(),
            source: "/dev/disk3s1".into(),
            local: fstype != "smbfs",
        };
        let table = Table::of(vec![
            mount("/", (1, 4), "apfs"),
            mount("/System/Volumes/VM", (1, 6), "apfs"),
            mount("/System/Volumes/Data", (1, 5), "apfs"),
            mount("/Volumes/Stick", (2, 1), "exfat"),
            mount("/Volumes/share", (0, 9), "smbfs"),
        ])
        .with_firmlinks(
            &skwird_crawl::mounts::firmlinks(
                b"/Users\tUsers\n/Applications\tApplications\n/usr/local\tusr/local\n",
            ),
            |_| true,
        );
        assert_eq!(
            firmlinked(&table),
            paths(&["/", "/Applications", "/Users", "/usr/local"])
        );
        // Before macOS 10.15, no Data volume: `/` alone.
        let older = Table::of(vec![mount("/", (1, 4), "apfs")]);
        assert_eq!(firmlinked(&older), paths(&["/"]));
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

    /// A group is a user's own private group if it is their primary
    /// group, no one else's, and names no other member.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_users_private_group_is_theirs_alone() {
        let passwd = "someone:x:1000:1000::/home/someone:/bin/sh\nother:x:1001:100::/home/other:/bin/sh\nthird:x:1002:100::/home/third:/bin/sh\n";
        let group = "someone:x:1000:\nusers:x:100:\nwheel:x:10:someone,other\n";
        assert!(is_private_group(passwd, group, 1000, 1000));
        assert!(
            !is_private_group(passwd, group, 1001, 100),
            "shared as primary"
        );
        assert!(
            !is_private_group(passwd, group, 1000, 10),
            "not their primary"
        );
        let joined = "someone:x:1000:other\n";
        assert!(
            !is_private_group(passwd, joined, 1000, 1000),
            "another member"
        );
        assert!(!is_private_group(passwd, group, 4242, 1000), "no such user");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn users_are_read_from_the_user_database() {
        let text = "root:x:0:0::/root:/bin/sh\nsomeone:x:1000:100:Some One:/home/someone:/bin/sh\nbroken\nnobody:x:65534:65534::/var/empty:/bin/false\n";
        assert_eq!(
            passwd_users(text),
            vec![
                (0, PathBuf::from("/root")),
                (1000, PathBuf::from("/home/someone")),
                (65534, PathBuf::from("/var/empty"))
            ]
        );
    }
}
