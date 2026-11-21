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
    for name in ["skwird-protocol", "skwird-client"] {
        let manifest = std::fs::read_to_string(vendor.join(name).join("Cargo.toml")).unwrap();
        assert!(!manifest.contains("workspace"), "{name}: {manifest}");
        assert!(vendor.join(name).join("VENDORED").is_file());
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let built = Command::new(cargo)
        .args(["check", "--offline", "--quiet", "--manifest-path"])
        .arg(vendor.join("skwird-client/Cargo.toml"))
        .env("CARGO_TARGET_DIR", s.join("target"))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "the copies do not build alone:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}
