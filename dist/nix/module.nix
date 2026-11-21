# NixOS's `services.skwird`: skwird installed, so that a client finds it
# to start, and with `session`, a user's engine for every user's session
# (docs/DESIGN.md, decision 1): each user's socket held by their service
# manager, which starts the engine at their first client and keeps it
# for the session, so that what it watches stays live between clients.
# The system service is phase 3's (docs/PLAN.md).
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.skwird;
in {
  options.services.skwird = {
    enable = lib.mkEnableOption "skwird, an index of file names kept current";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalMD "skwird's flake package for the system";
      description = "The skwird package to install.";
    };

    session = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        A user's engine for every user's session, started at their first
        client. Off, a client starts an engine itself where none answers,
        which goes a minute after its last client.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    # The package's units, in the user manager's path; their [Install]
    # applied here, which `systemd.packages` does not do.
    systemd.packages = lib.optional cfg.session cfg.package;
    systemd.user.sockets.skwird = lib.mkIf cfg.session {
      wantedBy = ["sockets.target"];
    };
  };
}
