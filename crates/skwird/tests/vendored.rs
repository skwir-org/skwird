//! The copies a client keeps of the client's crates build on their own
//! (`docs/PLAN.md`, phase 1, step 5): `tools/vendor-client` writes them
//! into a folder outside any workspace, and they build there, offline,
//! from what the registry already holds.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use skwird_testkit::Scratch;

#[test]
fn the_copies_of_the_clients_crates_build_on_their_own() {
    let s = Scratch::new("vendored");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let vendor = s.dir("vendor");
    let copy = || {
        let made = Command::new("bash")
            .arg(repo.join("tools/vendor-client"))
            .arg(&vendor)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
    };
    copy();
    // Copied again over a copy: what the script wrote is replaced, a
    // file the client keeps beside it stays.
    let own = s.write("vendor/skwird-client/README.skwir.md", b"the client's own");
    let stale = s.write("vendor/skwird-client/src/gone.rs", b"");
    copy();
    assert_eq!(std::fs::read(&own).unwrap(), b"the client's own");
    assert!(!stale.exists(), "what skwird no longer has is not kept");
    for name in ["skwird-protocol", "skwird-client"] {
        let manifest = std::fs::read_to_string(vendor.join(name).join("Cargo.toml")).unwrap();
        assert!(!manifest.contains("workspace"), "{name}: {manifest}");
        assert!(vendor.join(name).join("VENDORED").is_file());
    }
    let client = std::fs::read_to_string(vendor.join("skwird-client/Cargo.toml")).unwrap();
    assert!(
        client.contains("[target.'cfg(windows)'.dependencies]\nwindows-sys"),
        "its pipes on Windows: {client}"
    );
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // For this system, and for Windows, whose pipes take a dependency of
    // their own: checked, not linked, so no linker of Windows' is needed.
    for target in [None, Some("x86_64-pc-windows-gnu")] {
        let mut check = Command::new(&cargo);
        check
            .args(["check", "--offline", "--quiet", "--manifest-path"])
            .arg(vendor.join("skwird-client/Cargo.toml"))
            .env("CARGO_TARGET_DIR", s.join("target"));
        if let Some(target) = target {
            check.args(["--target", target]);
        }
        let built = check.output().unwrap();
        assert!(
            built.status.success(),
            "the copies do not build alone, for {target:?}:\n{}",
            String::from_utf8_lossy(&built.stderr)
        );
    }
}
