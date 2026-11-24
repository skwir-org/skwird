//! Removable volumes the system has mounted (`docs/DESIGN.md`, Protocol,
//! "Removable volumes"; open question 8): what a client may offer its
//! user to keep, each by its identity, never by where it is mounted. None
//! is indexed unasked.
//!
//! On Linux:
//! - **A block device's filesystem:** a mount's device numbers name it in
//!   sysfs (`/sys/dev/block/MAJOR:MINOR`). A FUSE program's (a phone over
//!   MTP, `GVfs`, rclone) or a network's names no block device, and is
//!   never one. A mount of a folder of a filesystem, not its root (a
//!   bind), is not the volume; a filesystem mounted twice is seen once.
//! - **Removable** where its disk says so (`removable`), or sits on a USB
//!   bus or an SD card's (an MMC host): USB hard drives and SSDs say they
//!   are not removable. A device mapper's (an encrypted volume) is its
//!   underlying devices': removable where one is. A loop device's never
//!   is: snaps' packages are mounted from them.
//! - **Its identity:** udev's name for it in `/dev/disk/by-uuid`, as
//!   blkid spells it: a UUID in lowercase with its dashes, FAT's and
//!   exFAT's serial as `XXXX-XXXX`, NTFS's in sixteen digits. One with
//!   none is seen, and never indexed.
//! - **Its label:** udev's, in `/dev/disk/by-label`, its escapes undone.
//!
//! Elsewhere, not yet: macOS and Windows say so.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
use std::path::Path;

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
pub fn seen(table: &Table) -> Result<Vec<Removable>, String> {
    Ok(seen_in(table, Path::new("/sys"), Path::new("/dev/disk")))
}

/// Not yet on this system.
#[cfg(not(target_os = "linux"))]
pub fn seen(_: &Table) -> Result<Vec<Removable>, String> {
    Err(format!(
        "removable volumes are not seen on {} yet",
        std::env::consts::OS
    ))
}

/// [`seen`], sysfs read from `sys`, udev's names from `disk`.
#[cfg(target_os = "linux")]
pub fn seen_in(table: &Table, sys: &Path, disk: &Path) -> Vec<Removable> {
    let mut seen: Vec<((u32, u32), Removable)> = Vec::new();
    let devices =
        std::fs::canonicalize(sys.join("devices")).unwrap_or_else(|_| sys.join("devices"));
    for m in table.mounts() {
        // No block device (major 0: tmpfs, FUSE, a network's), a loop
        // device's (7), or a folder of a filesystem mounted elsewhere.
        if m.dev.0 == 0 || m.dev.0 == 7 || m.network() || m.root != Path::new("/") {
            continue;
        }
        if seen.iter().any(|(dev, _)| *dev == m.dev) {
            continue;
        }
        let Ok(device) =
            std::fs::canonicalize(sys.join(format!("dev/block/{}:{}", m.dev.0, m.dev.1)))
        else {
            continue;
        };
        if !removable(sys, &devices, &device, 0) {
            continue;
        }
        let Some(name) = device.file_name() else {
            continue;
        };
        seen.push((
            m.dev,
            Removable {
                id: named(&disk.join("by-uuid"), name),
                point: m.point.clone(),
                fstype: m.fstype.clone(),
                label: named(&disk.join("by-label"), name).map(|l| unescaped(&l)),
            },
        ));
    }
    seen.into_iter().map(|(_, r)| r).collect()
}

/// Whether the block device at `device` (its sysfs folder, below
/// `devices`) is removable: its disk says so, or sits on a USB or MMC bus;
/// a device mapper's where one of what it is made of is. `depth` bounds a
/// chain of them.
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
                    std::fs::canonicalize(sys.join("class/block").join(slave))
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
    bus.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c.starts_with("usb") || c.starts_with("mmc")
    })
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
mod tests {
    use std::os::unix::fs::symlink;

    use skwird_testkit::Scratch;

    use super::*;
    use crate::mounts::Mount;

    fn mount(dev: (u32, u32), point: &str, fstype: &str, root: &str) -> Mount {
        Mount {
            id: u64::from(dev.0 * 1000 + dev.1),
            parent: 1,
            dev,
            root: root.into(),
            point: point.into(),
            fstype: fstype.into(),
            source: "/dev/synthetic".into(),
            local: true,
        }
    }

    /// A synthetic sysfs and udev's folders: a USB stick, an SD card with
    /// no identity, a card reader's disk that says it is removable, an
    /// encrypted volume on the stick, an internal disk, a loop device.
    fn machine(s: &Scratch) -> (PathBuf, PathBuf) {
        let sys = s.dir("sys");
        let devices = |rel: &str| s.dir(format!("sys/devices/{rel}"));
        let usb = devices("pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/block/sdb");
        std::fs::write(usb.join("removable"), "0\n").unwrap();
        for part in ["sdb1", "sdb2"] {
            s.write(format!("sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/block/sdb/{part}/partition"), b"1\n");
        }
        let card = devices("pci0000:00/0000:00:1e.0/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0");
        std::fs::write(card.join("removable"), "0\n").unwrap();
        s.write(
            "sys/devices/pci0000:00/0000:00:1e.0/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0/mmcblk0p1/partition",
            b"1\n",
        );
        let reader = devices("pci0000:00/0000:00:17.0/ata2/host1/block/sdc");
        std::fs::write(reader.join("removable"), "1\n").unwrap();
        s.write(
            "sys/devices/pci0000:00/0000:00:17.0/ata2/host1/block/sdc/sdc1/partition",
            b"1\n",
        );
        let internal = devices("pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1");
        std::fs::write(internal.join("removable"), "0\n").unwrap();
        s.write(
            "sys/devices/pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1/nvme0n1p2/partition",
            b"1\n",
        );
        s.dir("sys/devices/virtual/block/dm-0/slaves/sdb2");
        s.dir("sys/devices/virtual/block/dm-1/slaves/nvme0n1p2");
        s.dir("sys/devices/virtual/block/loop0");
        s.dir("sys/dev/block");
        s.dir("sys/class/block");
        let block = |dev: &str, rel: &str| {
            symlink(
                sys.join("devices").join(rel),
                sys.join("dev/block").join(dev),
            )
            .unwrap();
        };
        let usb_part = "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/block/sdb";
        block("8:17", &format!("{usb_part}/sdb1"));
        block("8:18", &format!("{usb_part}/sdb2"));
        block(
            "179:1",
            "pci0000:00/0000:00:1e.0/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0/mmcblk0p1",
        );
        block("8:33", "pci0000:00/0000:00:17.0/ata2/host1/block/sdc/sdc1");
        block(
            "259:2",
            "pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1/nvme0n1p2",
        );
        block("253:0", "virtual/block/dm-0");
        block("253:1", "virtual/block/dm-1");
        block("7:0", "virtual/block/loop0");
        for (name, rel) in [
            ("sdb2", format!("{usb_part}/sdb2")),
            (
                "nvme0n1p2",
                "pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1/nvme0n1p2".to_owned(),
            ),
        ] {
            symlink(
                sys.join("devices").join(rel),
                sys.join("class/block").join(name),
            )
            .unwrap();
        }
        let disk = s.dir("disk");
        s.dir("disk/by-uuid");
        s.dir("disk/by-label");
        for (dir, name, device) in [
            ("by-uuid", "ABCD-1234", "sdb1"),
            ("by-uuid", "0b6d3a4e-5f61-4c2a-9d7e-1a2b3c4d5e6f", "dm-0"),
            ("by-uuid", "5C2E8A1F3B9D7E40", "sdc1"),
            (
                "by-uuid",
                "f0e1d2c3-b4a5-4697-8879-6a5b4c3d2e1f",
                "nvme0n1p2",
            ),
            ("by-label", "SYNTHETIC\\x20STICK", "sdb1"),
        ] {
            symlink(format!("../../{device}"), disk.join(dir).join(name)).unwrap();
        }
        (sys, disk)
    }

    /// A USB stick, an SD card, a card reader's disk and an encrypted
    /// volume on the stick are seen, each by its identity where it has
    /// one; an internal disk, a loop device, a tmpfs, a FUSE program's and
    /// a folder mounted from the stick are not, and a filesystem mounted
    /// twice is seen once.
    #[test]
    fn removable_volumes_are_seen_by_their_identity() {
        let s = Scratch::new("removable");
        let (sys, disk) = machine(&s);
        let table = Table::of(vec![
            mount((259, 2), "/", "ext4", "/"),
            mount((0, 25), "/run", "tmpfs", "/"),
            mount((8, 17), "/media/someone/SYNTHETIC STICK", "vfat", "/"),
            mount((8, 17), "/mnt/again", "vfat", "/"),
            mount((8, 17), "/srv/a folder of it", "vfat", "/inside"),
            mount((179, 1), "/media/someone/card", "exfat", "/"),
            mount((8, 33), "/media/someone/reader", "ntfs3", "/"),
            mount((253, 0), "/media/someone/sealed", "ext4", "/"),
            mount((253, 1), "/home", "ext4", "/"),
            mount((7, 0), "/snap/a/1", "squashfs", "/"),
            mount((0, 60), "/run/user/1000/gvfs", "fuse.gvfsd-fuse", "/"),
        ]);
        let seen = seen_in(&table, &sys, &disk);
        let said: Vec<(Option<&str>, &str, &str, Option<&str>)> = seen
            .iter()
            .map(|r| {
                (
                    r.id.as_deref(),
                    r.point.to_str().unwrap(),
                    r.fstype.as_str(),
                    r.label.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            said,
            [
                (
                    Some("ABCD-1234"),
                    "/media/someone/SYNTHETIC STICK",
                    "vfat",
                    Some("SYNTHETIC STICK")
                ),
                (None, "/media/someone/card", "exfat", None),
                (
                    Some("5C2E8A1F3B9D7E40"),
                    "/media/someone/reader",
                    "ntfs3",
                    None
                ),
                (
                    Some("0b6d3a4e-5f61-4c2a-9d7e-1a2b3c4d5e6f"),
                    "/media/someone/sealed",
                    "ext4",
                    None
                ),
            ]
        );
    }

    #[test]
    fn a_label_is_unescaped_as_udev_escaped_it() {
        assert_eq!(unescaped("SYNTHETIC\\x20STICK"), "SYNTHETIC STICK");
        assert_eq!(unescaped("caf\\xc3\\xa9"), "caf\u{e9}");
        assert_eq!(unescaped("plain\\x2"), "plain\\x2");
    }
}
