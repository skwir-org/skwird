# The tests that need root (crates/*/tests/root*.rs), run as root in a
# virtual machine (AGENTS.md, Testing), with filesystems of its own that
# they may mark, mount and fill (skwird_testkit::Machine): an ext4 one,
# a btrfs one mounted through a subvolume, as a system's homes often are,
# holding two subvolumes of its own, and an ext4 one on a device whose
# every read waits 20 ms, as a spinning disk's seek does (dm-delay).
#
# One machine, two ways of giving it the tests:
# - `tests`, the binaries Nix built (the flake's `checks.root`, for
#   `nix flake check`);
# - none: the binaries cargo built on the host, in the folder
#   $SKWIRD_VM_TESTS shared into the machine (the flake's `root-machine`,
#   which tools/vm/root-tests runs).
{
  pkgs,
  tests ? null,
}: let
  shared = tests == null;
  dir =
    if shared
    then "/mnt/tests"
    else "${tests}/bin";
in
  pkgs.testers.runNixOSTest {
    name = "skwird-root";

    nodes.machine = {
      lib,
      pkgs,
      ...
    }: {
      virtualisation = {
        memorySize = 2048;
        cores = 4;
        # /dev/vdb, /dev/vdc and /dev/vdd, in MiB.
        emptyDiskImages = [1024 1024 256];
        # A USB controller, for the stick the test script plugs in.
        qemu.options = ["-device qemu-xhci,id=xhci"];
        sharedDirectories = lib.optionalAttrs shared {
          skwird-tests = {
            source = ''"$SKWIRD_VM_TESTS"'';
            target = "/mnt/tests";
          };
        };
      };
      boot.supportedFilesystems = ["btrfs"];
      boot.kernelModules = ["dm-delay"];
      environment.systemPackages = [pkgs.btrfs-progs pkgs.e2fsprogs pkgs.acl pkgs.lvm2 pkgs.dosfstools];
      environment.etc."skwird-test-machine".text = ''
        ext4=/mnt/ext4
        btrfs=/mnt/btrfs
        slow=/mnt/slow
        slow_device=/dev/mapper/slow
        stick=/dev/sda
      '';
    };

    testScript = ''
      import os
      machine.wait_for_unit("multi-user.target")
      # A USB stick of 64 MiB, plugged in as a user plugs one in: removable,
      # on the USB bus, the machine's one SCSI disk (/dev/sda).
      stick = os.path.join(os.environ.get("TMPDIR", "/tmp"), "stick.img")
      with open(stick, "wb") as f:
          f.truncate(64 << 20)
      machine.send_monitor_command(f"drive_add 0 if=none,id=stick,format=raw,file={stick}")
      machine.send_monitor_command("device_add usb-storage,bus=xhci.0,drive=stick,id=stick,removable=on")
      machine.wait_until_succeeds("test -b /dev/sda")
      machine.succeed(
          "mkfs.vfat -n 'TEST STICK' /dev/sda",
          "udevadm trigger --settle /dev/sda",
      )
      machine.succeed(
          "mkfs.ext4 -q /dev/vdb",
          "mkdir -p /mnt/ext4 && mount /dev/vdb /mnt/ext4",
          "mkfs.btrfs -q /dev/vdc",
          "mkdir -p /mnt/top /mnt/btrfs && mount /dev/vdc /mnt/top",
          "btrfs -q subvolume create /mnt/top/home && umount /mnt/top",
          "mount -o subvol=home /dev/vdc /mnt/btrfs",
          "btrfs -q subvolume create /mnt/btrfs/a",
          "btrfs -q subvolume create /mnt/btrfs/b",
          "dmsetup create slow --table \"0 $(blockdev --getsz /dev/vdd) delay /dev/vdd 0 20 /dev/vdd 0 0\"",
          "mkfs.ext4 -q /dev/mapper/slow",
          "mkdir -p /mnt/slow && mount /dev/mapper/slow /mnt/slow",
      )
      print(machine.succeed("uname -r"))
      tests = [t for t in machine.succeed("ls ${dir}").split() if t.startswith("root-")]
      assert tests, "no test binaries in ${dir}"
      for test in tests:
          with subtest(test):
              print(machine.succeed(f"cd /root && SKWIRD_TEST_MACHINE=1 SKWIRD_BIN=${dir}/skwird ${dir}/{test} --test-threads=1 2>&1"))
    '';
  }
