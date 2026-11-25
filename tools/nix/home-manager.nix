# The Home Manager module (dist/nix/home-manager.nix), checked by what it
# makes, as skwir checks its own (`checks.home-manager`):
# - on Linux, the exclusions declared written as the engine reads them,
#   and the package's own user units linked where the user's service
#   manager reads them, the socket wanted by `sockets.target`;
# - with `service` off, no unit, and no exclusions file where none is
#   declared;
# - inside NixOS, `service` off by default where NixOS runs skwird's
#   system service or every user's session, on where it runs neither;
# - on macOS, evaluated from here, the LaunchAgent `skwird logon add`
#   writes, under its label.
#
#   nix build .#checks.x86_64-linux.home-manager -L
{
  self,
  pkgs,
  nixpkgs,
  home-manager,
}: let
  inherit (pkgs) lib;
  homeWith = hmPkgs: skwird:
    (home-manager.lib.homeManagerConfiguration {
      pkgs = hmPkgs;
      modules = [
        self.homeManagerModules.default
        {
          home = {
            username = "someone";
            homeDirectory =
              if hmPkgs.stdenv.hostPlatform.isDarwin
              then "/Users/someone"
              else "/home/someone";
            stateVersion = "25.11";
          };
          services.skwird = {enable = true;} // skwird;
        }
      ];
    })
    .config;
  linux = homeWith pkgs {exclude = ["~/Videos" "/mnt/scratch"];};
  none = homeWith pkgs {service = false;};
  darwin = homeWith nixpkgs.legacyPackages.aarch64-darwin {};
  agent = darwin.launchd.agents.skwird.config;
  # This user's `service` where Home Manager runs inside NixOS, NixOS's
  # own `services.skwird` being `os`.
  insideNixos = os:
    (nixpkgs.lib.nixosSystem {
      inherit (pkgs.stdenv.hostPlatform) system;
      modules = [
        self.nixosModules.default
        home-manager.nixosModules.home-manager
        {
          services.skwird = {enable = true;} // os;
          users.users.someone.isNormalUser = true;
          home-manager.users.someone = {
            imports = [self.homeManagerModules.default];
            home.stateVersion = "25.11";
            services.skwird.enable = true;
          };
          boot.loader.grub.enable = false;
          fileSystems."/".device = "/dev/null";
          system.stateVersion = "25.11";
        }
      ];
    })
    .config
    .home-manager
    .users
    .someone
    .services
    .skwird
    .service;
  expect = what: holds:
    lib.optionalString (!holds) ''
      echo "not so: ${what}" >&2
      exit 1
    '';
  package = linux.services.skwird.package;
in
  pkgs.runCommand "skwird-home-manager" {} ''
    files=${linux.home-files}
    printf '# Declared by Home Manager (services.skwird.exclude).\n~/Videos\n/mnt/scratch\n' > expected
    cmp expected $files/.config/skwird/exclude
    units=$files/.config/systemd/user
    grep -qx 'ListenStream=%t/skwird/socket' $units/skwird.socket
    grep -qx 'ExecStart=${package}/bin/skwird serve --forever' $units/skwird.service
    grep -qx 'ListenStream=%t/skwird/socket' $units/sockets.target.wants/skwird.socket
    test ! -e $units/default.target.wants/skwird.service
    grep -qx 'Environment="XDG_CONFIG_HOME=/home/someone/.config"' $units/skwird.service.d/home-manager.conf
    grep -qx 'Environment="XDG_STATE_HOME=/home/someone/.local/state"' $units/skwird.service.d/home-manager.conf
    test -x ${package}/bin/skwird

    off=${none.home-files}
    test ! -e $off/.config/systemd/user/skwird.socket
    test ! -e $off/.config/systemd/user/sockets.target.wants
    test ! -e $off/.config/skwird/exclude

    ${expect "inside NixOS with its system service, the user's engine is off" (!insideNixos {system = true;})}
    ${expect "inside NixOS with every user's session, the user's engine is off" (!insideNixos {session = true;})}
    ${expect "inside NixOS running neither, the user's engine is on" (insideNixos {session = false;})}

    ${expect "macOS' agent is skwird logon's" (agent.Label == "org.skwir.skwird")}
    ${expect "macOS' agent serves for the session" (lib.drop 1 agent.ProgramArguments == ["serve" "--forever"])}
    ${expect "macOS' agent logs in the engine's state folder" (agent.StandardErrorPath == "/Users/someone/.local/state/skwird/engine.log")}
    ${expect "macOS' agent is told the user's folders" (agent.EnvironmentVariables.XDG_CONFIG_HOME == "/Users/someone/.config")}
    touch $out
  ''
