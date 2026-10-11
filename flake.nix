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
    nixpkgs,
    rust-overlay,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [rust-overlay.overlays.default];
      };

      # The toolchain rust-toolchain.toml pins. The Windows and macOS
      # targets check those builds from Linux (DESIGN 9: every system).
      pinned = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
      rustToolchain = pkgs.rust-bin.stable.${pinned}.default.override {
        extensions = ["rust-src" "rust-analyzer" "clippy" "rustfmt"];
        targets = ["x86_64-pc-windows-gnu" "aarch64-apple-darwin"];
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
    });
}
