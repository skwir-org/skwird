//! Removable volumes the system has mounted (`docs/DESIGN.md`, Protocol,
//! "Removable volumes"; open question 8): what a client may offer its
//! user to keep, each by its identity, never by where it is mounted. None
//! is indexed unasked.
//!
//! On Linux:
//! - **A block device's filesystem:** a mount's device numbers name it in
//!   sysfs (`/sys/dev/block/MAJOR:MINOR`); where they are anonymous, as a
//!   btrfs's always are (every mount of one btrfs shares them), its source
//!   does, a block device under `/dev` (`/dev/mapper/…`): two USB drives,
//!   LUKS with btrfs inside, were otherwise seen as none. The source is
//!   trusted for btrfs alone, and only where it is a block device's node:
//!   a tmpfs's, an overlay's or a FUSE program's source is free text. A
//!   FUSE program's (a phone over MTP, `GVfs`, rclone) or a network's
//!   names no block device, and is never one.
//! - **The volume's own mount:** a mount of a folder of a filesystem (a
//!   bind) is not the volume where the filesystem is mounted at its root
//!   too; a btrfs mounted only through a subvolume is its volume, as
//!   mounted. A filesystem mounted twice is seen once, and a mount another
//!   one later at the same point hides is not seen.
//! - **Removable** where its disk says so (`removable`), or sits on a USB
//!   bus, or is an SD card (an MMC host's card whose `type` is `SD`): USB
//!   hard drives and SSDs say they are not removable, and an eMMC soldered
//!   in the machine is an MMC host's too. A device mapper's (an encrypted
//!   volume) is its underlying devices': removable where one is. A loop
//!   device's never is: snaps' packages are mounted from them.
//! - **Never the running system's:** `/`, `/home`, `/boot`, `/usr` and
//!   the like are no removable volume, whatever their disk (a system
//!   booted from a USB disk or an SD card). The system service indexes
//!   them, and offers the rest to their users' engines ([`is_removable`]).
//! - **Its identity:** a btrfs's, the filesystem's own, in
//!   `/sys/fs/btrfs/<uuid>`, which every member device of it names (udev's
//!   `by-uuid` link of a btrfs of several devices names one alone);
//!   any other's, udev's name for it in `/dev/disk/by-uuid`, as blkid
//!   spells it: a UUID in lowercase with its dashes, FAT's and exFAT's
//!   serial as `XXXX-XXXX`, NTFS's in sixteen digits. One with none is
//!   seen, and never indexed.
//! - **Its label:** a btrfs's, the filesystem's own; any other's, udev's,
//!   in `/dev/disk/by-label`, its escapes undone.
//!
//! On macOS: each volume the Finder shows, from a disk `DiskArbitration`
//! says is plugged in ([`seen`]).
//!
//! On Windows: each volume mounted, at each path it is mounted at, whose
//! drive is removable (or optical), or whose disk is on a USB or SD bus or
//! a virtual disk attached (`crate::windows::plugged`).

use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use crate::mounts::Mount;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::mounts::Table;

/// A removable volume mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removable {
    /// Its identity, as this engine spells it; none where it has none,
    /// and it is never indexed.
    pub id: Option<String>,
    /// Where it is mounted.
    pub point: PathBuf,
    /// Its filesystem's type, as the system names it.
    pub fstype: String,
    /// Its label, as its filesystem holds it, for a client to show.
    pub label: Option<String>,
}

/// The removable volumes mounted, in the mount table's order.
#[cfg(target_os = "linux")]
pub fn mounted() -> Result<Vec<Removable>, String> {
    let table = Table::read().map_err(|e| format!("the mount table cannot be read: {e}"))?;
    Ok(seen_in(&table, Path::new("/sys"), Path::new("/dev")))
}

/// The removable volumes mounted, in the mount table's order.
#[cfg(target_os = "macos")]
pub fn mounted() -> Result<Vec<Removable>, String> {
    let table = Table::read().map_err(|e| format!("the mount table cannot be read: {e}"))?;
    Ok(seen(&table))
}

/// The removable volumes mounted, each where it is mounted, in the
/// system's order of volumes.
#[cfg(windows)]
pub fn mounted() -> Result<Vec<Removable>, String> {
    crate::sys::plugged::mounted()
}

/// The removable volumes mounted, in the mount table's order (macOS):
/// each the Finder shows, from a disk `DiskArbitration` says is plugged
/// in: its device not inside the machine, its media removable (an SD card
/// in a reader inside it), or a disk image. Its identity is its volume's
/// UUID, the one `DiskArbitration` derives from a FAT or exFAT serial; its
/// label, its volume's name.
#[cfg(target_os = "macos")]
fn seen(table: &Table) -> Vec<Removable> {
    let hidden = crate::sys::not_browsed();
    let mut seen: Vec<Removable> = Vec::new();
    for m in table.mounts() {
        if !m.local || hidden.contains(&m.point) || seen.iter().any(|r| r.point == m.point) {
            continue;
        }
        let Some(bsd) = m
            .source
            .to_str()
            .and_then(|s| s.strip_prefix("/dev/"))
            .filter(|b| b.starts_with("disk"))
        else {
            continue;
        };
        let Some(disk) = crate::sys::disks::describe(bsd) else {
            continue;
        };
        if !(!disk.internal || disk.removable || disk.protocol.as_deref() == Some("Disk Image")) {
            continue;
        }
        seen.push(Removable {
            id: disk.uuid,
            point: m.point.clone(),
            fstype: m.fstype.clone(),
            label: disk.name,
        });
    }
    seen
}

/// Not on this system.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn mounted() -> Result<Vec<Removable>, String> {
    Err(format!(
        "removable volumes are not seen on {} yet",
        std::env::consts::OS
    ))
}

/// Where the running system is mounted: never a removable volume, whatever
/// its disk.
#[cfg(target_os = "linux")]
const SYSTEM: &[&str] = &[
    "/",
    "/boot",
    "/boot/efi",
    "/efi",
    "/home",
    "/nix",
    "/nix/store",
    "/usr",
    "/var",
];

/// Whether mount `m` is a removable volume's, as [`mounted`] tells one
/// (Linux): what the system service leaves to its users' engines
/// (`docs/DESIGN.md`, open question 8).
#[cfg(target_os = "linux")]
pub fn is_removable(m: &Mount) -> bool {
    let sys = Path::new("/sys");
    let devices = crate::canonical(sys.join("devices")).unwrap_or_else(|_| sys.join("devices"));
    !SYSTEM.iter().any(|p| m.point == Path::new(p))
        && m.dev.0 != 7
        && !m.network()
        && block_device(m, sys, Path::new("/dev"), &block_numbers)
            .is_some_and(|d| removable(sys, &devices, &d, 0))
}

/// The removable volumes of `table`, sysfs read from `sys`, `/dev` from
/// `dev`, udev's names from its `disk`.
#[cfg(target_os = "linux")]
pub fn seen_in(table: &Table, sys: &Path, dev: &Path) -> Vec<Removable> {
    seen_with(table, sys, dev, &block_numbers)
}

/// The numbers of the block device whose node is at `node`, links
/// followed; none where it is no block device's (a file, a folder, a
/// character device).
#[cfg(target_os = "linux")]
fn block_numbers(node: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::metadata(node).ok()?;
    if !meta.file_type().is_block_device() {
        return None;
    }
    let rdev = meta.rdev();
    Some((rustix::fs::major(rdev), rustix::fs::minor(rdev)))
}

/// [`seen_in`], a block device's numbers told by `numbers` (a test's own
/// devices are no nodes of the system's).
#[cfg(target_os = "linux")]
fn seen_with(
    table: &Table,
    sys: &Path,
    dev: &Path,
    numbers: &dyn Fn(&Path) -> Option<(u32, u32)>,
) -> Vec<Removable> {
    let disk = dev.join("disk");
    let devices = crate::canonical(sys.join("devices")).unwrap_or_else(|_| sys.join("devices"));
    let mounts = table.mounts();
    // Each mount's block device, where it has one and is no loop device's,
    // network's or the running system's; none for a mount a later one at
    // the same point hides.
    let of: Vec<Option<PathBuf>> = mounts
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let hidden = mounts[i + 1..].iter().any(|later| later.point == m.point);
            let system = SYSTEM.iter().any(|p| m.point == Path::new(p));
            if hidden || system || m.dev.0 == 7 || m.network() {
                return None;
            }
            block_device(m, sys, dev, numbers)
        })
        .collect();
    let mut seen: Vec<(PathBuf, Removable)> = Vec::new();
    for (i, m) in mounts.iter().enumerate() {
        let Some(device) = &of[i] else { continue };
        // Seen once, at its first mount.
        if seen.iter().any(|(d, _)| d == device) {
            continue;
        }
        // A folder of it mounted apart is not the volume where it is
        // mounted at its root too, nor ever but a btrfs's subvolume.
        let whole = m.root == Path::new("/");
        let rooted_elsewhere = mounts
            .iter()
            .zip(&of)
            .any(|(o, d)| o.root == Path::new("/") && d.as_ref() == Some(device));
        if !whole && (rooted_elsewhere || m.fstype != "btrfs") {
            continue;
        }
        if !removable(sys, &devices, device, 0) {
            continue;
        }
        let Some(name) = device.file_name() else {
            continue;
        };
        let btrfs = (m.fstype == "btrfs")
            .then(|| btrfs_named(sys, name))
            .flatten();
        let (id, label) = match btrfs {
            Some(named) => named,
            None => (
                named(&disk.join("by-uuid"), name),
                named(&disk.join("by-label"), name).map(|l| unescaped(&l)),
            ),
        };
        seen.push((
            device.clone(),
            Removable {
                id,
                point: m.point.clone(),
                fstype: m.fstype.clone(),
                label,
            },
        ));
    }
    seen.into_iter().map(|(_, r)| r).collect()
}

/// The sysfs folder of the block device mount `m` is a filesystem of, `dev`
/// standing for `/dev`, `numbers` telling a node's. Its device numbers
/// name it; where they are anonymous (major 0), a btrfs's always, its
/// source does, a path below `/dev` that holds no `..`, whose node is a
/// block device's (`/dev/mapper/luks-…`, a link to `/dev/dm-1`). Any
/// other filesystem's anonymous numbers name no device: a tmpfs's,
/// an overlay's or a FUSE program's source is free text. A btrfs of
/// several devices names one of them, enough to tell whether it is
/// removable.
#[cfg(target_os = "linux")]
fn block_device(
    m: &Mount,
    sys: &Path,
    dev: &Path,
    numbers: &dyn Fn(&Path) -> Option<(u32, u32)>,
) -> Option<PathBuf> {
    let (major, minor) = if m.dev.0 != 0 {
        m.dev
    } else {
        if m.fstype != "btrfs" {
            return None;
        }
        let below = Path::new(&m.source).strip_prefix("/dev").ok()?;
        let plain = below
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !plain || below.as_os_str().is_empty() {
            return None;
        }
        numbers(&dev.join(below))?
    };
    crate::canonical(sys.join(format!("dev/block/{major}:{minor}"))).ok()
}

/// A btrfs's identity and label, its own: the filesystem in
/// `/sys/fs/btrfs` among whose devices the one named `device` is (`sdd`,
/// `dm-3`). None where none is.
#[cfg(target_os = "linux")]
fn btrfs_named(sys: &Path, device: &std::ffi::OsStr) -> Option<(Option<String>, Option<String>)> {
    std::fs::read_dir(sys.join("fs/btrfs"))
        .ok()?
        .flatten()
        .find_map(|fs| {
            let uuid = fs.file_name().to_string_lossy().into_owned();
            // A filesystem's folder is named by its UUID; `features` is
            // beside them.
            let is_uuid = uuid.len() == 36 && uuid.bytes().filter(|&b| b == b'-').count() == 4;
            if !is_uuid || !fs.path().join("devices").join(device).exists() {
                return None;
            }
            let label = std::fs::read_to_string(fs.path().join("label"))
                .ok()
                .map(|l| l.trim_end_matches('\n').to_owned())
                .filter(|l| !l.is_empty());
            Some((Some(uuid), label))
        })
}

/// Whether the block device at `device` (its sysfs folder, below
/// `devices`) is removable: its disk says so, or sits on a USB bus, or is
/// an SD card; a device mapper's where one of what it is made of is.
/// `depth` bounds a chain of them.
#[cfg(target_os = "linux")]
fn removable(sys: &Path, devices: &Path, device: &Path, depth: u32) -> bool {
    let name = device.file_name().map(|n| n.to_string_lossy().into_owned());
    if name.as_deref().is_some_and(|n| n.starts_with("loop")) {
        return false;
    }
    if let Ok(slaves) = std::fs::read_dir(device.join("slaves")) {
        let under: Vec<PathBuf> = slaves.flatten().map(|e| e.file_name().into()).collect();
        if !under.is_empty() {
            return depth < 8
                && under.iter().any(|slave| {
                    crate::canonical(sys.join("class/block").join(slave))
                        .is_ok_and(|d| removable(sys, devices, &d, depth + 1))
                });
        }
    }
    let disk = if device.join("partition").exists() {
        device.parent().unwrap_or(device)
    } else {
        device
    };
    if std::fs::read_to_string(disk.join("removable")).is_ok_and(|r| r.trim() == "1") {
        return true;
    }
    let bus = disk.strip_prefix(devices).unwrap_or(disk);
    if bus
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with("usb"))
    {
        return true;
    }
    // An MMC host's card: an SD card, or an eMMC soldered in the machine,
    // told by the card's `type` (`…/mmc0:0001/block/mmcblk0`).
    let mmc = bus
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with("mmc"));
    mmc && disk
        .parent()
        .and_then(Path::parent)
        .and_then(|card| std::fs::read_to_string(card.join("type")).ok())
        .is_some_and(|t| t.trim() == "SD")
}

/// The name in udev's folder `dir` whose link leads to the device named
/// `device` (`../../sdb1`).
#[cfg(target_os = "linux")]
fn named(dir: &Path, device: &std::ffi::OsStr) -> Option<String> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let target = std::fs::read_link(e.path()).ok()?;
        (target.file_name()? == device).then(|| e.file_name().to_string_lossy().into_owned())
    })
}

/// A name udev escaped (`MY\x20STICK`), as it was.
#[cfg(target_os = "linux")]
fn unescaped(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && let Some(hex) = name.get(i + 2..i + 4)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
