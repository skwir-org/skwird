# The user service in a virtual machine (AGENTS.md, Testing): skwird's
# package and NixOS module on a machine of their own, with two synthetic
# users whose service managers run from boot. Run with
#
#   nix build .#checks.x86_64-linux.user-service -L
#
# or by hand, the machine's shell open in Python:
#
#   nix run .#checks.x86_64-linux.user-service.driverInteractive
#
# Every client below sets SKWIRD_NO_START, so it never starts an engine
# itself: what answers is the service's, or nothing.
{
  self,
  pkgs,
}:
pkgs.testers.runNixOSTest {
  name = "skwird-user-service";

  nodes.machine = {
    imports = [self.nixosModules.default];
    services.skwird.enable = true;
    users.users = {
      someone = {
        isNormalUser = true;
        uid = 1000;
        linger = true;
      };
      other = {
        isNormalUser = true;
        uid = 1001;
        linger = true;
      };
    };
  };

  testScript = ''
    import shlex

    def client(user, uid, command):
        env = f"XDG_RUNTIME_DIR=/run/user/{uid} SKWIRD_NO_START=1"
        return f"su - {user} -c {shlex.quote(env + ' ' + command)}"

    def someone(command):
        return machine.succeed(client("someone", 1000, command))

    def manager(command):
        return machine.succeed(f"systemctl --user -M someone@ {command}").strip()

    covered = "skwird coverage ~ | grep -Eq '^names: (checked|as of|live)'"

    machine.wait_for_unit("skwird.socket", "someone")

    with subtest("the socket waits for a client, and starts no engine before one"):
        machine.fail("systemctl --user -M someone@ is-active skwird.service")

    with subtest("the first client starts the service's engine, which answers"):
        someone("mkdir -p ~/docs/a && touch ~/docs/a/'note one.txt' ~/docs/'café.md'")
        machine.wait_until_succeeds(client("someone", 1000, covered))
        found = someone("skwird find 'note one' --in ~")
        assert found.splitlines() == ["/home/someone/docs/a/note one.txt"], found
        found = someone("skwird find café --in ~")
        assert found.splitlines() == ["/home/someone/docs/café.md"], found
        # One engine, the service's.
        main = manager("show -p MainPID --value skwird.service")
        engines = machine.succeed("pgrep -x -u someone skwird").split()
        assert engines == [main], (engines, main)

    with subtest("a folder watched is answered live, with no sync"):
        said = someone("skwird watch ~/docs")
        assert ": live since " in said, said
        someone("touch ~/docs/a/'heard later.txt'")
        machine.wait_until_succeeds(
            client("someone", 1000, "skwird find 'heard later' --in ~/docs 2>&1 | grep -q 'names live since'")
            + " && " + client("someone", 1000, "skwird find 'heard later' --in ~/docs | grep -qx /home/someone/docs/a/'heard later.txt'"),
            timeout=30,
        )

    with subtest("the engine restarted watches what was registered again"):
        manager("restart skwird.service")
        machine.wait_until_succeeds(
            client("someone", 1000, "skwird coverage ~/docs | grep -q '^names: live since '")
        )

    with subtest("each user's engine is their own, and their socket theirs alone"):
        machine.wait_for_unit("skwird.socket", "other")
        machine.succeed(client("other", 1001, "touch ~/mine.txt"))
        machine.wait_until_succeeds(client("other", 1001, covered))
        found = machine.succeed(client("other", 1001, "skwird find mine --in ~"))
        assert found.splitlines() == ["/home/other/mine.txt"], found
        machine.fail(client("other", 1000, "skwird status"))
  '';
}
