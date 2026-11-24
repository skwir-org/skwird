//! The system's mount table, and which mounts reach a network
//! (`docs/DESIGN.md`, decision 2).
//!
//! Mounts are known from the table, never by reading a path: a stat of a
//! dead server's mount point blocks. The table is read again at each
//! crawl and check (phase 1), and followed from phase 2.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// A mount, as the table lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub id: u64,
    pub parent: u64,
    /// Its device's major and minor numbers.
    pub dev: (u32, u32),
    /// The folder of its filesystem it shows.
    pub root: PathBuf,
    /// Where it is mounted.
    pub point: PathBuf,
    pub fstype: String,
    pub source: OsString,
    /// Whether the system says it is on this machine: macOS does
    /// (`MNT_LOCAL`); Linux says nothing, and every mount is taken so there.
    pub local: bool,
}

impl Mount {
    /// Whether it reaches a network ([`network`]), or the system says it
    /// is not on this machine.
    pub fn network(&self) -> bool {
        !self.local || network(&self.fstype, &self.source)
    }
}

/// The mounts of this process's namespace, in the table's order: a mount
/// listed later at the same point is above the earlier.
#[derive(Clone, Debug, Default)]
pub struct Table {
    mounts: Vec<Mount>,
}

impl Table {
    /// The table as `/proc/self/mountinfo` writes it. A line it cannot
    /// read is left out.
    pub fn parse(text: &[u8]) -> Table {
        Table {
            mounts: text.split(|&b| b == b'\n').filter_map(parse_line).collect(),
        }
    }

    /// This process's mount table.
    #[cfg(target_os = "linux")]
    pub fn read() -> std::io::Result<Table> {
        std::fs::read("/proc/self/mountinfo").map(|text| Table::parse(&text))
    }

    /// The system's mount table, as macOS holds it (`getfsstat`).
    #[cfg(target_os = "macos")]
    pub fn read() -> std::io::Result<Table> {
        crate::sys::mounts().map(|mounts| Table { mounts })
    }

    /// This process's mount table: Linux's and macOS' alone so far.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn read() -> std::io::Result<Table> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "skwird reads Linux's and macOS' mount tables only so far",
        ))
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    pub fn by_id(&self, id: u64) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.id == id)
    }

    /// The mount that holds `path`: the one at the longest point above
    /// it, the topmost there. Told from the path's spelling alone, so no
    /// path is read: `path` should be absolute and hold no `..`.
    pub fn holding(&self, path: &Path) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|m| path.starts_with(&m.point))
            .max_by_key(|m| m.point.components().count())
            .and_then(|longest| self.mounts.iter().rev().find(|m| m.point == longest.point))
    }

    /// Every point strictly below `root` where a mount starts, relative
    /// to `root`, and whether the topmost mount there reaches a network.
    pub fn below(&self, root: &Path) -> HashMap<PathBuf, bool> {
        let mut points = HashMap::new();
        for m in &self.mounts {
            if let Ok(rel) = m.point.strip_prefix(root) {
                if rel.as_os_str().is_empty() {
                    continue;
                }
                // A later mount at the same point is above the earlier.
                points.insert(rel.to_path_buf(), m.network());
            }
        }
        points
    }
}

fn parse_line(line: &[u8]) -> Option<Mount> {
    // `36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw`:
    // the fields before `-` are id, parent, major:minor, root, mount
    // point, options and optional fields; after it, the type, the source
    // and the filesystem's options.
    let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    let dash = fields.iter().position(|f| *f == b"-")?;
    if dash < 6 || fields.len() < dash + 3 {
        return None;
    }
    let number = |f: &[u8]| std::str::from_utf8(f).ok()?.parse::<u64>().ok();
    let (major, minor) = std::str::from_utf8(fields[2]).ok()?.split_once(':')?;
    Some(Mount {
        id: number(fields[0])?,
        parent: number(fields[1])?,
        dev: (major.parse().ok()?, minor.parse().ok()?),
        root: PathBuf::from(os(unescape(fields[3]))),
        point: PathBuf::from(os(unescape(fields[4]))),
        fstype: String::from_utf8_lossy(&unescape(fields[dash + 1])).into_owned(),
        source: os(unescape(fields[dash + 2])),
        local: true,
    })
}

/// A field with the table's escapes undone: `\040` is a space, and any
/// byte may be written so, in octal.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && let Some(octal) = field.get(i + 1..i + 4)
            && octal.iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let value = octal.iter().fold(0u32, |n, &b| n * 8 + u32::from(b - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

/// The kernel's network filesystems. The same list as skwir's
/// (`skwir_core::fs::reach`), so that both tell a network mount alike.
const KERNEL_NETWORK: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "ceph",
    "afs",
    "coda",
    "ncpfs",
    "lustre",
    "glusterfs",
    "gpfs",
    "beegfs",
    "orangefs",
    "pvfs2",
    "davfs",
];

/// FUSE's filesystems that reach a network, by their subtype
/// (`fuse.sshfs`). The same list as skwir's (`skwir_core::fs::reach`).
const FUSE_NETWORK: &[&str] = &[
    "sshfs",
    "rclone",
    "gvfsd-fuse",
    "kio-fuse",
    // A phone that KDE's kmtpd holds, mounted by skwir's helper: waited
    // on as a remote waits on its server.
    "skwir-kmtp",
    "curlftpfs",
    "s3fs",
    "gcsfuse",
    "goofys",
    "geesefs",
    "davfs",
    "davfs2",
    "smbnetfs",
    "afpfs",
    "afpfs-ng",
    "glusterfs",
    "ceph-fuse",
    "juicefs",
    "blobfuse",
    "blobfuse2",
    "mountpoint-s3",
    "ossfs",
    "onedriver",
    "google-drive-ocamlfuse",
    "seaweedfs",
    "cvmfs2",
    "httpdirfs",
    "httpfs2",
];

/// Whether a mount of `fstype` from `source` reaches a network, as skwir
/// tells it (`skwir_core::fs::reach::network`): one of the kernel's
/// network filesystems, a FUSE subtype known to reach one, or a FUSE
/// source that names a server (`me@host:/srv`, `host:path`).
pub fn network(fstype: &str, source: &OsStr) -> bool {
    if KERNEL_NETWORK.contains(&fstype) {
        return true;
    }
    let Some(subtype) = fstype.strip_prefix("fuse.") else {
        return false;
    };
    if FUSE_NETWORK.contains(&subtype) {
        return true;
    }
    let source = source.to_string_lossy();
    !source.starts_with('/')
        && source
            .split_once(':')
            .is_some_and(|(server, _)| !server.is_empty() && !server.contains('/'))
}

/// `path` as the system names it, told from its spelling alone: doubled
/// and trailing separators and `.` dropped, a Windows path's prefix (its
/// drive or share) kept. `None` if it is not absolute or holds `..`, which
/// only reading the path could resolve.
pub fn lexical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            // The first component alone, as `components` gives it.
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(c),
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => return None,
        }
    }
    path.is_absolute().then_some(out)
}

/// A name's bytes as a path component, for callers that build paths
/// from listings.
#[cfg(unix)]
pub fn component(name: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(OsStr::from_bytes(name))
}

/// Bytes of the table as a name: as they are on Unix, read as UTF-8
/// elsewhere.
#[cfg(unix)]
fn os(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os(bytes: Vec<u8>) -> OsString {
    String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[u8] = b"\
22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/mapper/root rw,subvol=/@
36 22 0:88 / /run/user/1000/gvfs rw,nosuid shared:922 - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000
40 22 0:90 / /mnt/nas rw shared:9 - nfs4 nas:/export rw
41 22 0:21 /@home /home rw,relatime shared:2 - btrfs /dev/mapper/root rw,subvol=/@home
50 41 0:95 / /home/user/My\\040Drive rw - fuse.rclone gdrive: rw
51 41 0:96 / /home/user/archive rw - fuse.someday /home/user/archive.zip rw
52 41 0:97 / /home/user/stick rw - vfat /dev/sdb1 rw
53 52 0:98 / /home/user/stick rw - tmpfs none rw
this line is not a mount
";

    /// Each field is read as the kernel writes it, escapes undone, and a
    /// line that is not a mount is left out.
    #[test]
    fn the_table_is_read_as_the_kernel_writes_it() {
        let table = Table::parse(TABLE);
        assert_eq!(table.mounts().len(), 8);
        let home = table.by_id(41).unwrap();
        assert_eq!(home.parent, 22);
        assert_eq!(home.dev, (0, 21));
        assert_eq!(home.root, Path::new("/@home"));
        assert_eq!(home.point, Path::new("/home"));
        assert_eq!(home.fstype, "btrfs");
        let drive = table.by_id(50).unwrap();
        assert_eq!(drive.point, Path::new("/home/user/My Drive"));
        assert_eq!(drive.source, OsStr::new("gdrive:"));
    }

    /// A network mount is told by its type, or by a FUSE source that names
    /// a server, as skwir's test tells it.
    #[test]
    fn network_mounts_are_told_by_their_type_and_source() {
        let os = OsStr::new;
        assert!(network("nfs4", os("nas:/export")));
        assert!(network("cifs", os("//nas/photos")));
        assert!(network("fuse.sshfs", os("me@host:/")));
        assert!(network("fuse.rclone", os("gdrive:")));
        assert!(network("fuse.gvfsd-fuse", os("gvfsd-fuse")));
        assert!(
            network("fuse.someday", os("me@server:/srv")),
            "a server's source"
        );
        assert!(!network("fuseblk", os("/dev/sde1")), "ntfs-3g");
        assert!(!network("fuse.portal", os("portal")));
        assert!(!network("fuse.someday", os("/home/me/archive.zip")));
        assert!(!network("btrfs", os("/dev/nvme0n1p2")));
        assert!(!network("tmpfs", os("tmpfs")));
    }

    /// Below a root are the points under it, not the root's own, each
    /// told network or not by the mount on top.
    #[test]
    fn the_points_below_a_root_are_the_topmost_mounts_under_it() {
        let table = Table::parse(TABLE);
        let below = table.below(Path::new("/home/user"));
        assert_eq!(below.len(), 3);
        assert_eq!(below.get(Path::new("My Drive")), Some(&true));
        assert_eq!(below.get(Path::new("archive")), Some(&false));
        assert_eq!(
            below.get(Path::new("stick")),
            Some(&false),
            "the tmpfs on top"
        );
        assert!(table.below(Path::new("/home/user/stick")).is_empty());
        assert!(
            table.below(Path::new("/home/us")).is_empty(),
            "a point is held by components, not by bytes"
        );
    }

    /// The mount holding a path is the deepest above it, the topmost
    /// there, told from the path's spelling.
    #[test]
    fn the_mount_holding_a_path_is_the_deepest_above_it() {
        let table = Table::parse(TABLE);
        let id = |p: &str| table.holding(Path::new(p)).map(|m| m.id);
        assert_eq!(id("/home/user/notes"), Some(41));
        assert_eq!(id("/home/user/stick/a"), Some(53));
        assert_eq!(id("/mnt/nas/photos"), Some(40));
        assert_eq!(id("/etc"), Some(22));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_is_named_as_the_system_names_it() {
        let p = |s: &str| lexical(Path::new(s));
        assert_eq!(p("/a//b/"), Some(PathBuf::from("/a/b")));
        assert_eq!(p("/a/./b"), Some(PathBuf::from("/a/b")));
        assert_eq!(p("/a/../b"), None);
        assert_eq!(p("a/b"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_path_is_named_as_the_system_names_it() {
        let p = |s: &str| lexical(Path::new(s));
        assert_eq!(p(r"C:\a\\b\"), Some(PathBuf::from(r"C:\a\b")));
        assert_eq!(p(r"C:\a\.\b"), Some(PathBuf::from(r"C:\a\b")));
        assert_eq!(p("C:/a/b"), Some(PathBuf::from(r"C:\a\b")));
        assert_eq!(
            p(r"\\server\share\a\"),
            Some(PathBuf::from(r"\\server\share\a"))
        );
        assert_eq!(p(r"C:\a\..\b"), None);
        assert_eq!(p(r"a\b"), None);
        // Below the current drive's root: which drive, only the process
        // knows.
        assert_eq!(p(r"\a"), None);
    }
}
