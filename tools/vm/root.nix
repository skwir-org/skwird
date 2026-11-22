# The tests that need root (crates/*/tests/root*.rs), run as root in a
# virtual machine (AGENTS.md, Testing), with filesystems of its own that
# they may mark, mount and fill (skwird_testkit::Machine): an ext4 one,
# and a btrfs one mounted through a subvolume, as a system's homes often
# are, holding two subvolumes of its own.
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
        # /dev/vdb and /dev/vdc, in MiB.
        emptyDiskImages = [1024 1024];
        sharedDirectories = lib.optionalAttrs shared {
          skwird-tests = {
            source = ''"$SKWIRD_VM_TESTS"'';
            target = "/mnt/tests";
          };
        };
      };
      boot.supportedFilesystems = ["btrfs"];
      environment.systemPackages = [pkgs.btrfs-progs pkgs.e2fsprogs];
      environment.etc."skwird-test-machine".text = ''
        ext4=/mnt/ext4
        btrfs=/mnt/btrfs
      '';
    };

    testScript = ''
      machine.wait_for_unit("multi-user.target")
      machine.succeed(
          "mkfs.ext4 -q /dev/vdb",
          "mkdir -p /mnt/ext4 && mount /dev/vdb /mnt/ext4",
          "mkfs.btrfs -q /dev/vdc",
          "mkdir -p /mnt/top /mnt/btrfs && mount /dev/vdc /mnt/top",
          "btrfs -q subvolume create /mnt/top/home && umount /mnt/top",
          "mount -o subvol=home /dev/vdc /mnt/btrfs",
          "btrfs -q subvolume create /mnt/btrfs/a",
          "btrfs -q subvolume create /mnt/btrfs/b",
      )
      print(machine.succeed("uname -r"))
      tests = machine.succeed("ls ${dir}").split()
      assert tests, "no test binaries in ${dir}"
      for test in tests:
          with subtest(test):
              print(machine.succeed(f"cd /root && SKWIRD_TEST_MACHINE=1 ${dir}/{test} --test-threads=1 2>&1"))
    '';
  }
