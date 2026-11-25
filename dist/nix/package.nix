# skwird, built as nixpkgs builds a package, as skwir's is (its ADR-124):
# the engine and its command line, with its user service's units where
# the system's service manager looks for them.
#
# Three arguments are the flake's own: `src` (its git tree), `version`
# (the skwird crate's) and `cargoLock` (its Cargo.lock). A copy in
# nixpkgs gives `fetchFromGitHub`, a literal version and `cargoHash` in
# their place; the rest is written to stand there as it is.
{
  lib,
  rustPlatform,
  # The flake's (see above).
  src,
  version,
  cargoLock,
}:
rustPlatform.buildRustPackage {
  pname = "skwird";
  inherit version src cargoLock;

  # The engine and its command line, one program; the measuring tool
  # (skwird-bench) is the repository's, not the package's.
  cargoBuildFlags = [
    "--package=skwird"
    # skwirdw is Windows' (skwird with no console window).
    "--bin=skwird"
  ];
  # The tests need a real filesystem's behaviour and a cache folder of
  # their own (tools/gate.sh); those that need root run in the flake's
  # virtual machines (`checks`), never in a build.
  doCheck = false;

  postInstall = ''
    # A user's engine for the session (phase 2): the socket starts it at
    # the first client.
    install -Dm644 dist/systemd/skwird.service dist/systemd/skwird.socket -t $out/lib/systemd/user
    substituteInPlace $out/lib/systemd/user/skwird.service \
      --replace-fail /usr/bin/skwird $out/bin/skwird
    # The system service (phase 3): every user answered what they could
    # list.
    install -Dm644 dist/systemd/skwird-system.service -t $out/lib/systemd/system
    substituteInPlace $out/lib/systemd/system/skwird-system.service \
      --replace-fail /usr/bin/skwird $out/bin/skwird
    install -Dm644 dist/sysusers/skwird.conf -t $out/lib/sysusers.d
    install -Dm644 LICENSE-MIT LICENSE-APACHE -t $out/share/licenses/skwird
  '';

  # The toolchain stays out of what is installed: rustc writes the paths
  # of std's sources into panic messages when they are in its sysroot.
  disallowedReferences = [
    rustPlatform.rust.rustc
    rustPlatform.rust.cargo
  ];

  meta = {
    description = "An index of file names kept current by the system's change notifications, answered over a local socket";
    license = with lib.licenses; [
      mit
      asl20
    ];
    # Linux's and macOS' (phase 5); Windows' is built with MinGW
    # (tools/release), not here. On macOS, its systemd units are unused:
    # Home Manager's module makes a LaunchAgent there.
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
    mainProgram = "skwird";
  };
}
