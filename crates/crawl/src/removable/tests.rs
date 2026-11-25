use std::os::unix::fs::symlink;

use skwird_testkit::Scratch;

use super::*;

fn mount(dev: (u32, u32), point: &str, fstype: &str, root: &str) -> Mount {
    mount_from(dev, point, fstype, root, "/dev/synthetic")
}

fn mount_from(dev: (u32, u32), point: &str, fstype: &str, root: &str, source: &str) -> Mount {
    Mount {
        id: u64::from(dev.0 * 1000 + dev.1),
        parent: 1,
        dev,
        root: root.into(),
        point: point.into(),
        fstype: fstype.into(),
        source: source.into(),
        local: true,
    }
}

/// The block devices of the synthetic `/dev`, by their nodes' names: its
/// nodes are plain files, which no system's call says are a block
/// device's.
fn numbers(node: &Path) -> Option<(u32, u32)> {
    let name = crate::canonical(node).ok()?;
    match name.file_name()?.to_str()? {
        "dm-3" => Some((253, 3)),
        "dm-4" => Some((253, 4)),
        "nvme0n1p3" => Some((259, 3)),
        "sdf" => Some((8, 80)),
        "sdh" => Some((8, 112)),
        _ => None,
    }
}

const USB2: &str = "pci0000:00/0000:00:14.0/usb2";
const USB3: &str = "pci0000:00/0000:00:14.0/usb3";
const NVME: &str = "pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1";

/// A synthetic sysfs and `/dev`, udev's folders in it:
/// - a USB stick (sdb) of two partitions, the second holding an encrypted
///   volume (dm-0);
/// - an SD card (mmcblk0) with no identity, and an eMMC (mmcblk1), each
///   on an MMC host, told apart by their cards' `type`;
/// - a card reader's disk (sdc) that says it is removable;
/// - an internal disk (nvme0n1), holding an encrypted volume (dm-1) and a
///   btrfs (nvme0n1p3);
/// - two USB drives, LUKS with btrfs inside, one on a whole disk (sdd,
///   dm-3), one on a partition (sde2, dm-4);
/// - a btrfs of two USB disks (sdf, sdg), whose udev link names sdg;
/// - a USB disk (sdh) holding a btrfs mounted through a subvolume alone;
/// - a loop device.
#[allow(clippy::too_many_lines)] // One machine's sysfs and /dev, written out.
fn machine(s: &Scratch) -> (PathBuf, PathBuf) {
    let sys = s.dir("sys");
    let devices = |rel: &str| s.dir(format!("sys/devices/{rel}"));
    let with = |rel: &str, file: &str, text: &str| {
        s.write(format!("sys/devices/{rel}/{file}"), text.as_bytes());
    };
    let usb = format!("{USB2}/2-1/2-1:1.0/host6/block/sdb");
    devices(&usb);
    with(&usb, "removable", "0\n");
    for part in ["sdb1", "sdb2"] {
        with(&format!("{usb}/{part}"), "partition", "1\n");
    }
    let card = "pci0000:00/0000:00:1e.0/mmc_host/mmc0/mmc0:aaaa";
    with(card, "type", "SD\n");
    with(&format!("{card}/block/mmcblk0"), "removable", "0\n");
    with(
        &format!("{card}/block/mmcblk0/mmcblk0p1"),
        "partition",
        "1\n",
    );
    let emmc = "pci0000:00/0000:00:1f.0/mmc_host/mmc1/mmc1:0001";
    with(emmc, "type", "MMC\n");
    with(&format!("{emmc}/block/mmcblk1"), "removable", "0\n");
    with(
        &format!("{emmc}/block/mmcblk1/mmcblk1p2"),
        "partition",
        "2\n",
    );
    let reader = "pci0000:00/0000:00:17.0/ata2/host1/block/sdc";
    with(reader, "removable", "1\n");
    with(&format!("{reader}/sdc1"), "partition", "1\n");
    with(NVME, "removable", "0\n");
    for part in ["nvme0n1p2", "nvme0n1p3"] {
        with(&format!("{NVME}/{part}"), "partition", "1\n");
    }
    let whole = format!("{USB3}/3-1/3-1:1.0/host7/block/sdd");
    with(&whole, "removable", "0\n");
    let parted = format!("{USB3}/3-2/3-2:1.0/host8/block/sde");
    with(&parted, "removable", "0\n");
    with(&format!("{parted}/sde2"), "partition", "2\n");
    let raid_a = format!("{USB3}/3-3/3-3:1.0/host9/block/sdf");
    let raid_b = format!("{USB3}/3-4/3-4:1.0/host10/block/sdg");
    let subvolumed = format!("{USB3}/3-5/3-5:1.0/host11/block/sdh");
    for disk in [&raid_a, &raid_b, &subvolumed] {
        with(disk, "removable", "0\n");
    }
    for (mapper, under) in [
        ("dm-0", "sdb2"),
        ("dm-1", "nvme0n1p2"),
        ("dm-3", "sdd"),
        ("dm-4", "sde2"),
    ] {
        s.dir(format!("sys/devices/virtual/block/{mapper}/slaves/{under}"));
    }
    devices("virtual/block/loop0");
    s.dir("sys/dev/block");
    s.dir("sys/class/block");
    let block = |numbers: &str, rel: &str| {
        symlink(
            sys.join("devices").join(rel),
            sys.join("dev/block").join(numbers),
        )
        .unwrap();
    };
    block("8:17", &format!("{usb}/sdb1"));
    block("8:18", &format!("{usb}/sdb2"));
    block("179:1", &format!("{card}/block/mmcblk0/mmcblk0p1"));
    block("179:258", &format!("{emmc}/block/mmcblk1/mmcblk1p2"));
    block("8:33", &format!("{reader}/sdc1"));
    block("259:2", &format!("{NVME}/nvme0n1p2"));
    block("259:3", &format!("{NVME}/nvme0n1p3"));
    block("253:0", "virtual/block/dm-0");
    block("253:1", "virtual/block/dm-1");
    block("253:3", "virtual/block/dm-3");
    block("253:4", "virtual/block/dm-4");
    block("8:80", &raid_a);
    block("8:112", &subvolumed);
    block("7:0", "virtual/block/loop0");
    for (name, rel) in [
        ("sdb2", format!("{usb}/sdb2")),
        ("nvme0n1p2", format!("{NVME}/nvme0n1p2")),
        ("sdd", whole.clone()),
        ("sde2", format!("{parted}/sde2")),
    ] {
        symlink(
            sys.join("devices").join(rel),
            sys.join("class/block").join(name),
        )
        .unwrap();
    }
    // The btrfs filesystems, by their UUIDs, each its devices and label.
    s.dir("sys/fs/btrfs/features");
    for (uuid, members, label) in [
        (
            "3a4b5c6d-7e8f-4a1b-9c2d-0e1f2a3b4c5d",
            &["dm-3"][..],
            "WHOLE DRIVE\n",
        ),
        ("4b5c6d7e-8f9a-4b2c-8d3e-1f2a3b4c5d6e", &["dm-4"][..], "\n"),
        (
            "5c6d7e8f-9a0b-4c3d-9e4f-2a3b4c5d6e7f",
            &["nvme0n1p3"][..],
            "internal\n",
        ),
        (
            "6d7e8f9a-0b1c-4d2e-8f3a-4b5c6d7e8f9a",
            &["sdf", "sdg"][..],
            "TWO DISKS\n",
        ),
        (
            "7e8f9a0b-1c2d-4e3f-9a4b-5c6d7e8f9a0b",
            &["sdh"][..],
            "SUBVOLUMED\n",
        ),
    ] {
        for member in members {
            s.dir(format!("sys/fs/btrfs/{uuid}/devices/{member}"));
        }
        s.write(format!("sys/fs/btrfs/{uuid}/label"), label.as_bytes());
    }
    let dev = s.dir("dev");
    let disk = s.dir("dev/disk");
    s.dir("dev/disk/by-uuid");
    s.dir("dev/disk/by-label");
    s.dir("dev/mapper");
    for node in [
        "dm-3",
        "dm-4",
        "nvme0n1p3",
        "sdf",
        "sdg",
        "sdh",
        "not-a-device",
    ] {
        s.write(format!("dev/{node}"), b"");
    }
    for (name, node) in [("luks-whole", "dm-3"), ("luks-parted", "dm-4")] {
        symlink(format!("../{node}"), dev.join("mapper").join(name)).unwrap();
    }
    for (dir, name, device) in [
        ("by-uuid", "ABCD-1234", "sdb1"),
        ("by-uuid", "0b6d3a4e-5f61-4c2a-9d7e-1a2b3c4d5e6f", "dm-0"),
        ("by-uuid", "5C2E8A1F3B9D7E40", "sdc1"),
        (
            "by-uuid",
            "f0e1d2c3-b4a5-4697-8879-6a5b4c3d2e1f",
            "nvme0n1p2",
        ),
        (
            "by-uuid",
            "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
            "mmcblk1p2",
        ),
        // udev's link of the btrfs of two disks names the other one.
        ("by-uuid", "6d7e8f9a-0b1c-4d2e-8f3a-4b5c6d7e8f9a", "sdg"),
        ("by-label", "SYNTHETIC\\x20STICK", "sdb1"),
    ] {
        symlink(format!("../../{device}"), disk.join(dir).join(name)).unwrap();
    }
    (sys, dev)
}

/// What is seen of `table`: each volume's identity, mount point,
/// filesystem and label.
fn said(table: Vec<Mount>) -> Vec<(Option<String>, String, String, Option<String>)> {
    let s = Scratch::new("removable");
    let (sys, dev) = machine(&s);
    seen_with(&Table::of(table), &sys, &dev, &numbers)
        .into_iter()
        .map(|r| {
            (
                r.id,
                r.point.to_string_lossy().into_owned(),
                r.fstype,
                r.label,
            )
        })
        .collect()
}

fn volume(
    id: Option<&str>,
    point: &str,
    fstype: &str,
    label: Option<&str>,
) -> (Option<String>, String, String, Option<String>) {
    (
        id.map(str::to_owned),
        point.to_owned(),
        fstype.to_owned(),
        label.map(str::to_owned),
    )
}

/// A USB stick, an SD card, a card reader's disk and an encrypted volume
/// on the stick are seen, each by its identity where it has one. An
/// internal disk, an eMMC, a loop device, a tmpfs, a FUSE program's, a
/// folder mounted from the stick (listed before the stick itself, so that
/// seeing each device once does not hide it), a stick mounted where the
/// running system is, and one a later mount hides, are not; a filesystem
/// mounted twice is seen once.
#[test]
fn removable_volumes_are_seen_by_their_identity() {
    let seen = said(vec![
        mount((259, 2), "/", "ext4", "/"),
        mount_from((0, 25), "/run", "tmpfs", "/", "tmpfs"),
        mount((8, 17), "/srv/a folder of it", "vfat", "/inside"),
        mount((8, 17), "/media/someone/SYNTHETIC STICK", "vfat", "/"),
        mount((8, 17), "/mnt/again", "vfat", "/"),
        mount((179, 1), "/media/someone/card", "exfat", "/"),
        mount((179, 258), "/media/someone/soldered", "ext4", "/"),
        mount((8, 33), "/media/someone/reader", "ntfs3", "/"),
        mount((253, 0), "/media/someone/sealed", "ext4", "/"),
        mount((253, 1), "/data", "ext4", "/"),
        mount((8, 18), "/var", "ext4", "/"),
        mount((8, 18), "/media/someone/covered", "ext4", "/"),
        mount_from((0, 26), "/media/someone/covered", "tmpfs", "/", "tmpfs"),
        mount((7, 0), "/snap/a/1", "squashfs", "/"),
        mount_from(
            (0, 60),
            "/run/user/1000/gvfs",
            "fuse.gvfsd-fuse",
            "/",
            "gvfsd-fuse",
        ),
    ]);
    assert_eq!(
        seen,
        [
            volume(
                Some("ABCD-1234"),
                "/media/someone/SYNTHETIC STICK",
                "vfat",
                Some("SYNTHETIC STICK")
            ),
            volume(None, "/media/someone/card", "exfat", None),
            volume(
                Some("5C2E8A1F3B9D7E40"),
                "/media/someone/reader",
                "ntfs3",
                None
            ),
            volume(
                Some("0b6d3a4e-5f61-4c2a-9d7e-1a2b3c4d5e6f"),
                "/media/someone/sealed",
                "ext4",
                None
            ),
        ]
    );
}

/// A btrfs's mounts have numbers of their own (major 0), shared by every
/// mount of it, never its device's: two USB drives, LUKS with btrfs
/// inside, one on a whole disk and one on a partition, are seen by their
/// sources' devices, named and labelled by their filesystems; one mounted
/// twice is seen once, and a subvolume of it mounted apart (listed first)
/// is not the volume. A btrfs of the internal disk is not seen.
#[test]
fn a_btrfs_on_luks_is_seen_by_its_source() {
    let whole = "/dev/mapper/luks-whole";
    let seen = said(vec![
        mount_from((0, 76), "/mnt/snapshots", "btrfs", "/@snapshots", whole),
        mount_from((0, 76), "/run/media/someone/WHOLE", "btrfs", "/", whole),
        mount_from((0, 76), "/mnt/whole again", "btrfs", "/", whole),
        mount_from(
            (0, 98),
            "/run/media/someone/PARTED",
            "btrfs",
            "/",
            "/dev/mapper/luks-parted",
        ),
        mount_from((0, 30), "/data", "btrfs", "/", "/dev/nvme0n1p3"),
    ]);
    assert_eq!(
        seen,
        [
            volume(
                Some("3a4b5c6d-7e8f-4a1b-9c2d-0e1f2a3b4c5d"),
                "/run/media/someone/WHOLE",
                "btrfs",
                Some("WHOLE DRIVE")
            ),
            volume(
                Some("4b5c6d7e-8f9a-4b2c-8d3e-1f2a3b4c5d6e"),
                "/run/media/someone/PARTED",
                "btrfs",
                None
            ),
        ]
    );
}

/// A btrfs of two USB disks is named by its filesystem, whichever disk its
/// mount's source and udev's link name; a btrfs mounted through a
/// subvolume alone is its volume, as mounted.
#[test]
fn a_btrfs_is_named_by_its_filesystem_and_seen_through_a_subvolume() {
    let seen = said(vec![
        mount_from((0, 80), "/run/media/someone/TWO", "btrfs", "/", "/dev/sdf"),
        mount_from((0, 81), "/run/media/someone/SUB", "btrfs", "/@", "/dev/sdh"),
    ]);
    assert_eq!(
        seen,
        [
            volume(
                Some("6d7e8f9a-0b1c-4d2e-8f3a-4b5c6d7e8f9a"),
                "/run/media/someone/TWO",
                "btrfs",
                Some("TWO DISKS")
            ),
            volume(
                Some("7e8f9a0b-1c2d-4e3f-9a4b-5c6d7e8f9a0b"),
                "/run/media/someone/SUB",
                "btrfs",
                Some("SUBVOLUMED")
            ),
        ]
    );
}

/// An anonymous number's source is trusted for btrfs alone, and only
/// where it is a block device's node with no `..` in its path: a tmpfs or
/// a FUSE program naming a USB drive's node, a btrfs whose source is a
/// plain file, and one whose source climbs out of `/dev`, are not seen.
#[test]
fn an_anonymous_source_is_trusted_for_a_btrfs_device_alone() {
    let whole = "/dev/mapper/luks-whole";
    let seen = said(vec![
        mount_from((0, 25), "/mnt/tmpfs", "tmpfs", "/", whole),
        mount_from((0, 61), "/mnt/fuse", "fuse.sshfs", "/", "/dev/sdf"),
        mount_from((0, 62), "/mnt/file", "btrfs", "/", "/dev/not-a-device"),
        mount_from(
            (0, 63),
            "/mnt/climbing",
            "btrfs",
            "/",
            "/dev/../dev/mapper/luks-whole",
        ),
    ]);
    assert_eq!(seen, []);
}

#[test]
fn a_label_is_unescaped_as_udev_escaped_it() {
    assert_eq!(unescaped("SYNTHETIC\\x20STICK"), "SYNTHETIC STICK");
    assert_eq!(unescaped("caf\\xc3\\xa9"), "caf\u{e9}");
    assert_eq!(unescaped("plain\\x2"), "plain\\x2");
}
