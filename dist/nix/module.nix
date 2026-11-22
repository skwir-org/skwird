# NixOS's `services.skwird`: skwird installed, so that a client finds it
# to start, and either:
# - with `system`, the system service (docs/DESIGN.md, decisions 1 and 4,
#   phase 3): every local filesystem marked whole, every user answered
#   what they could list, from boot; or
# - with `session`, a user's engine for every user's session: each user's
#   socket held by their service manager, which starts the engine at
#   their first client and keeps it for the session, so that what it
#   watches stays live between clients.
# A client finds the system service first, so with it, no user's engine
# is needed: `session` is off by default then, and nothing that watches
# runs twice for one user (rule 4).
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

    system = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        The system service, from boot: every local filesystem that stores
        files (the Nix store left out) marked whole with fanotify, and every
        user answered what they could list. It runs as a user of its own
        with `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`.
      '';
    };

    session = lib.mkOption {
      type = lib.types.bool;
      default = !cfg.system;
      defaultText = lib.literalExpression "!config.services.skwird.system";
      description = ''
        A user's engine for every user's session, started at their first
        client. Off, a client starts an engine itself where none answers,
        which goes a minute after its last client.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    # The package's units, in the service managers' paths; their [Install]
    # applied here, which `systemd.packages` does not do.
    systemd.packages = lib.optional (cfg.session || cfg.system) cfg.package;
    systemd.user.sockets.skwird = lib.mkIf cfg.session {
      wantedBy = ["sockets.target"];
    };
    systemd.services.skwird-system = lib.mkIf cfg.system {
      wantedBy = ["multi-user.target"];
    };
    # The service's user of its own (the unit's `User=skwird`).
    users.users.skwird = lib.mkIf cfg.system {
      isSystemUser = true;
      group = "skwird";
      description = "skwird, the system's index of file names";
    };
    users.groups.skwird = lib.mkIf cfg.system {};
  };
}
