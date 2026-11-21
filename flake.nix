{
  description = "skwird: an index of file names, kept current by the system's change notifications";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
  }:
    {
      # `services.skwird` for NixOS (dist/nix/module.nix): skwird installed,
      # and a user's engine for every user's session.
      nixosModules.default = import ./dist/nix/module.nix self;
      overlays.default = final: _: {
        skwird = self.packages.${final.stdenv.hostPlatform.system}.default;
      };
    }
    # Linux, and Apple's ARM Macs for macOS work (phase 5), as skwir's:
    # nixpkgs has dropped Intel Macs.
    // flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux" "aarch64-darwin"] (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [rust-overlay.overlays.default];
      };
      inherit (pkgs) lib stdenv;

      # The toolchain rust-toolchain.toml pins. The Windows and macOS
      # targets check those builds from Linux (DESIGN 9: every system).
      pinned = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
      rustToolchain = pkgs.rust-bin.stable.${pinned}.default.override {
        extensions = ["rust-src" "rust-analyzer" "clippy" "rustfmt"];
        targets = ["x86_64-pc-windows-gnu" "aarch64-apple-darwin"];
      };

      # The package (`packages.default`, dist/nix/package.nix), made as
      # skwir's is: built from what the build reads of the git tree (the
      # workspace, the units, the licences), so a change to the docs or the
      # tools builds nothing again; with the pinned Rust alone, no rust-src
      # in its sysroot, so no binary names the toolchain (the package
      # refuses one that does).
      rustMinimal = pkgs.rust-bin.stable.${pinned}.minimal;
      skwird = pkgs.callPackage ./dist/nix/package.nix {
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustMinimal;
          rustc = rustMinimal;
        };
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./crates
            ./dist/systemd
            ./LICENSE-MIT
            ./LICENSE-APACHE
          ];
        };
        # The skwird crate's: the workspace has no version of its own.
        inherit ((builtins.fromTOML (builtins.readFile ./crates/skwird/Cargo.toml)).package) version;
        cargoLock.lockFile = ./Cargo.lock;
      };
    in {
      # The C compiler that links comes with mkShell's stdenv.
      devShells.default = pkgs.mkShell {
        nativeBuildInputs = [
          rustToolchain
          pkgs.shellcheck # tools/measure.sh
        ];
        RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        RUST_BACKTRACE = "1";
        shellHook = ''
          echo "skwird dev shell: $(rustc --version)"
        '';
      };

      formatter = pkgs.nixpkgs-fmt;

      # Linux's engine alone so far: macOS's is phase 5's.
      packages = lib.optionalAttrs stdenv.hostPlatform.isLinux {
        default = skwird;
      };

      # What needs a machine of its own runs in a virtual one, never on the
      # developer's system (AGENTS.md, Testing): QEMU, with KVM where the
      # processor's virtualization is on, emulated (many times slower)
      # where it is not.
      checks = lib.optionalAttrs stdenv.hostPlatform.isLinux {
        user-service = import ./tools/vm/user-service.nix {inherit self pkgs;};
      };
    });
}
