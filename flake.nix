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
          ./dist/sysusers
          ./dist/windows
          ./dist/macos
          ./LICENSE-MIT
          ./LICENSE-APACHE
        ];
      };
      # The workspace's, every crate's (docs/DESIGN.md, decision 11).
      inherit ((builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package) version;
      cargoLock.lockFile = ./Cargo.lock;
      skwird = pkgs.callPackage ./dist/nix/package.nix {
        inherit rustPlatform src version cargoLock;
      };

      # The tests that need root (crates/*/tests/root*.rs), built by Nix for
      # the machine that runs them (`checks.root`).
      rootTests = rustPlatform.buildRustPackage {
        pname = "skwird-root-tests";
        inherit version src cargoLock;
        nativeBuildInputs = [pkgs.jq];
        buildPhase = ''
          runHook preBuild
          cargo test --offline --no-run --workspace --test 'root*' \
            --message-format=json-render-diagnostics > artifacts.json
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p $out/bin
          jq -r 'select(.reason == "compiler-artifact" and .executable != null and (.profile.test or .target.name == "skwird")) | .executable' \
            artifacts.json | while read -r exe; do cp "$exe" $out/bin/; done
          runHook postInstall
        '';
        doCheck = false;
      };

      # Windows' tests, built with MinGW and run under Wine
      # (tools/wine-tests, docs/PLAN.md phase 4): Wine is x86_64 Linux's
      # alone in nixpkgs.
      mingw = pkgs.pkgsCross.mingwW64;
      withWine = stdenv.hostPlatform.isLinux && stdenv.hostPlatform.isx86_64;
    in {
      # The C compiler that links comes with mkShell's stdenv.
      devShells.default = pkgs.mkShell ({
          nativeBuildInputs =
            [
              rustToolchain
              pkgs.shellcheck # tools/measure.sh
              pkgs.jq # tools/vm/root-tests
            ]
            ++ lib.optionals withWine [
              mingw.stdenv.cc
              pkgs.wineWow64Packages.stable
            ];
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          RUST_BACKTRACE = "1";
          shellHook = ''
            echo "skwird dev shell: $(rustc --version)"
          '';
        }
        // lib.optionalAttrs withWine {
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "x86_64-w64-mingw32-gcc";
          # Rust's std for windows-gnu links MinGW's winpthreads.
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib";
        });

      # What a release builds beside Linux's packages (tools/release,
      # docs/DESIGN.md, decision 11): macOS' program from Linux, as skwir's
      # is (its ADR-075), with the Apple SDK its `just macos-sdk` keeps in
      # ~/.local/share/skwir-build/macos-sdk (Apple's licence the user's,
      # never in the tree); clang itself and LLVM's Mach-O linker, which the
      # SDK's wrappers call (`XMAC_CLANG`, `XMAC_LLD`), named, not on the
      # PATH; rcodesign, to sign it ad hoc; zip, for Windows' programs.
      devShells.release = self.devShells.${system}.default.overrideAttrs (old: {
        nativeBuildInputs =
          old.nativeBuildInputs
          ++ [
            pkgs.rcodesign
            pkgs.zip
          ];
        XMAC_CLANG = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
        XMAC_LLD = "${pkgs.lld}/bin/ld64.lld";
      });

      formatter = pkgs.nixpkgs-fmt;

      # Linux's engine alone so far: macOS's is phase 5's.
      packages = lib.optionalAttrs stdenv.hostPlatform.isLinux {
        default = skwird;
        # The root tests' machine, given cargo's binaries from the host
        # (tools/vm/root-tests).
        root-machine = (import ./tools/vm/root.nix {inherit pkgs;}).driver;
      };

      # What needs a machine of its own runs in a virtual one, never on the
      # developer's system (AGENTS.md, Testing): QEMU, with KVM where the
      # processor's virtualization is on, emulated (many times slower)
      # where it is not.
      checks = lib.optionalAttrs stdenv.hostPlatform.isLinux {
        user-service = import ./tools/vm/user-service.nix {inherit self pkgs;};
        system-service = import ./tools/vm/system-service.nix {inherit self pkgs;};
        root = import ./tools/vm/root.nix {
          inherit pkgs;
          tests = rootTests;
        };
      };
    });
}
