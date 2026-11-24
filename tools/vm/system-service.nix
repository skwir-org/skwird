# The system service in a virtual machine (docs/PLAN.md, phase 3, step 7):
# skwird's package and NixOS module, `services.skwird.system`, on a machine
# of their own with two synthetic users, one in a group. Run with
#
#   nix build .#checks.x86_64-linux.system-service -L
#
# Every client sets SKWIRD_NO_START, so it never starts an engine itself:
# what answers is the system service, which every client finds first.
{
  self,
  pkgs,
}:
pkgs.testers.runNixOSTest {
  name = "skwird-system-service";

  nodes.machine = {
    imports = [self.nixosModules.default];
    services.skwird = {
      enable = true;
      system = true;
    };
    # /dev/vdb, mounted while the service runs.
    virtualisation.emptyDiskImages = [256];
    users.groups.team.gid = 2000;
    users.users = {
      someone = {
        isNormalUser = true;
        uid = 1000;
        extraGroups = ["team"];
      };
      other = {
        isNormalUser = true;
        uid = 1001;
      };
    };
  };

  testScript = ''
    import shlex

    def as_user(user, command):
        return machine.succeed(f"su - {user} -c {shlex.quote('SKWIRD_NO_START=1 ' + command)}")

    def lines(text):
        return [l for l in text.splitlines() if l]

    machine.wait_for_unit("skwird-system.service")
    machine.wait_until_succeeds("SKWIRD_NO_START=1 skwird coverage /home | grep -q '^names: live since '", timeout=120)

    with subtest("the service runs as a user of its own, with two capabilities alone"):
        pid = machine.succeed("systemctl show -p MainPID --value skwird-system.service").strip()
        status = machine.succeed(f"cat /proc/{pid}/status")
        uid = [l for l in status.splitlines() if l.startswith("Uid:")][0].split()[1]
        assert uid != "0", status
        caps = [l for l in status.splitlines() if l.startswith("CapEff:")][0].split()[1]
        # CAP_DAC_READ_SEARCH (2) and CAP_SYS_ADMIN (21).
        assert int(caps, 16) == (1 << 2) | (1 << 21), caps
        machine.fail("su - someone -c 'ls /var/cache/skwird/'")

    with subtest("every client finds the system service"):
        said = as_user("someone", "skwird status")
        assert "System scope" in said, said

    with subtest("a change is heard live, and answered to whom may list it"):
        as_user("someone", "mkdir -p ~/notes && echo x > ~/notes/'the plan.txt'")
        machine.wait_until_succeeds(
            "su - someone -c \"SKWIRD_NO_START=1 skwird find 'the plan' --in ~ | grep -q 'the plan.txt'\""
        )
        found = lines(as_user("someone", "skwird find 'the plan' --in ~"))
        assert found == ["/home/someone/notes/the plan.txt"], found
        said = as_user("someone", "skwird find 'the plan' --in ~ 2>&1 >/dev/null")
        assert "names live since" in said, said
        # Their home is theirs alone (0700): the other user is answered
        # nothing there, and told it is not covered for them.
        assert lines(as_user("other", "skwird find 'the plan' --in /home")) == []
        said = as_user("other", "skwird coverage /home/someone")
        assert "names: not covered: Denied" in said, said
        found = lines(machine.succeed("SKWIRD_NO_START=1 skwird find 'the plan' --in /home"))
        assert found == ["/home/someone/notes/the plan.txt"], found
        # Totals, per asker: the owner is counted their file; the other
        # user, who may not list the home, is told it is denied, and
        # nothing in it is counted.
        said = as_user("someone", "skwird totals ~")
        assert any(l.endswith(" /home/someone/notes") and " 1 files" in l for l in lines(said)), said
        said = as_user("other", "skwird totals /home")
        home = [l for l in lines(said) if l.endswith(" /home/someone")]
        assert home and " 0 files" in home[0], said
        assert "not covered: /home/someone (Denied)" in said, said

    with subtest("a folder its group may read is answered to the group alone"):
        machine.succeed(
            "mkdir -p /srv/team && chgrp team /srv/team && chmod 0750 /srv/team",
            "echo x > /srv/team/'for the team.txt'",
        )
        machine.wait_until_succeeds(
            "su - someone -c \"SKWIRD_NO_START=1 skwird find 'for the team' --in /srv | grep -q team\""
        )
        assert lines(as_user("other", "skwird find 'for the team' --in /srv")) == []

    with subtest("private folders are never indexed, a user's own exclusions too"):
        as_user(
            "someone",
            "mkdir -p ~/.ssh ~/diary ~/.config/skwird"
            " && echo x > ~/.ssh/id_test && echo x > ~/diary/'dear diary.txt'"
            " && echo '~/diary' > ~/.config/skwird/exclude",
        )
        # The user's file holds from the next sync, as their own engine's.
        as_user("someone", "skwird sync ~")
        for name in ["id_test", "dear diary"]:
            found = lines(machine.succeed(f"SKWIRD_NO_START=1 skwird find '{name}' --in /home"))
            assert found == [], (name, found)
        # Asked there, the user is told it is excluded, so that their client
        # walks it itself.
        said = as_user("someone", "skwird coverage ~/diary")
        assert "names: not covered: Excluded" in said, said

    with subtest("a filesystem mounted while it runs is a root, let go of once unmounted"):
        machine.succeed(
            "mkfs.ext4 -q /dev/vdb",
            "mkdir -p /srv/later",
            "mount /dev/vdb /srv/later",
            "echo x > '/srv/later/on a disk mounted later.txt'",
        )
        machine.wait_until_succeeds(
            "SKWIRD_NO_START=1 skwird find 'mounted later' --in /srv/later | grep -q 'mounted later.txt'",
            timeout=30,
        )
        said = machine.succeed("SKWIRD_NO_START=1 skwird coverage /srv/later")
        assert "names: live since " in said, said
        machine.succeed("umount /srv/later")
        machine.wait_until_succeeds(
            "test -z \"$(SKWIRD_NO_START=1 skwird find 'mounted later' --in /srv)\"",
            timeout=30,
        )
  '';
}
