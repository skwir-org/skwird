# Home Manager's `services.skwird`: skwird installed for one user, and
# either:
# - with `service`, their engine for the session: on Linux, the package's
#   own user units (dist/systemd), the socket held by their service
#   manager, which starts the engine at their first client and keeps it for
#   the session; on macOS, a LaunchAgent of theirs, started at their login,
#   the one `skwird logon add` writes (dist/macos/org.skwir.skwird.plist),
#   under the same label, so that the two are one agent;
# - or none: a client starts an engine where none answers, which goes a
#   minute after its last client.
# With `exclude`, their own exclusions, declared: the file the engine reads
# (`$XDG_CONFIG_HOME/skwird/exclude`, docs/DESIGN.md, decision 5).
#
# The system service needs root: it is NixOS's (`services.skwird.system`,
# dist/nix/module.nix). Where Home Manager runs inside NixOS and NixOS
# runs skwird already, its system service or every user's session,
# `service` is off by default: nothing that watches runs twice for one
# user (rule 4). Standalone, Home Manager cannot see the system's: turn
# `service` off where the system runs skwird's system service.
self: {
  config,
  lib,
  pkgs,
  osConfig ? null,
  ...
}: let
  cfg = config.services.skwird;
  inherit (pkgs.stdenv.hostPlatform) isLinux isDarwin;
  # NixOS's own `services.skwird`, where Home Manager runs inside NixOS.
  system = lib.attrByPath ["services" "skwird"] null (
    if osConfig == null
    then {}
    else osConfig
  );
  ranBySystem = system != null && system.enable && (system.system || system.session);
  units = "${cfg.package}/lib/systemd/user";
in {
  options.services.skwird = {
    enable = lib.mkEnableOption "skwird, an index of file names kept current, for this user";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalMD "skwird's flake package for the system";
      description = "The skwird package to install.";
    };

    service = lib.mkOption {
      type = lib.types.bool;
      default = !ranBySystem;
      defaultText = lib.literalMD ''
        `true`, but where Home Manager runs inside NixOS and NixOS runs
        skwird's system service or every user's session
      '';
      description = ''
        This user's engine for the session: on Linux, the package's systemd
        user socket and service, the engine started at the first client
        and kept; on macOS, a LaunchAgent started at login. Off, a client
        starts an engine itself where none answers, which goes a minute
        after its last client.
      '';
    };

    exclude = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      example = ["~/Videos" "~/.local/share/Steam" "/mnt/scratch"];
      description = ''
        Folders kept out of this user's index, written to
        {file}`$XDG_CONFIG_HOME/skwird/exclude`, one a line: an absolute
        path, or one below the home as `~/…`. Kept out, a folder is never
        indexed, not merely left out of answers; the private folders
        (`.ssh`, `.gnupg` and the like) are kept out whatever this says.
        None declared, the file is left as it is, the user's to write.
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {home.packages = [cfg.package];}

    (lib.mkIf (cfg.exclude != []) {
      xdg.configFile."skwird/exclude".text =
        "# Declared by Home Manager (services.skwird.exclude).\n"
        + lib.concatMapStrings (path: path + "\n") cfg.exclude;
    })

    # The package's own units, linked where the user's service manager
    # reads them, the socket wanted by `sockets.target`, as `systemctl
    # --user enable skwird.socket` makes it.
    (lib.mkIf (cfg.service && isLinux) {
      xdg.configFile = {
        "systemd/user/skwird.socket".source = "${units}/skwird.socket";
        "systemd/user/skwird.service".source = "${units}/skwird.service";
        "systemd/user/sockets.target.wants/skwird.socket".source = "${units}/skwird.socket";
      };
    })

    (lib.mkIf (cfg.service && isDarwin) {
      launchd.agents.skwird = {
        enable = true;
        config = {
          Label = "org.skwir.skwird";
          ProgramArguments = [(lib.getExe cfg.package) "serve" "--forever"];
          RunAtLoad = true;
          # Started again where it failed, as systemd's Restart=on-failure.
          KeepAlive.SuccessfulExit = false;
          # A crawl yields to what the user is doing.
          ProcessType = "Background";
          LowPriorityIO = true;
          Nice = 10;
          StandardErrorPath = "${config.xdg.stateHome}/skwird/engine.log";
        };
      };
      # launchd opens the log; its folder, the engine's state folder, is
      # made first.
      home.activation.skwirdState = lib.hm.dag.entryBefore ["setupLaunchAgents"] ''
        run mkdir -p ${lib.escapeShellArg "${config.xdg.stateHome}/skwird"}
      '';
    })
  ]);
}
