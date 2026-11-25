//! What the client's integration tests share. Each test binary takes
//! what it uses of it.

// What one test binary leaves of it another uses: no dead code.
#![allow(dead_code)]

use std::path::PathBuf;

/// A folder of the test's own, removed when dropped, by a test that failed
/// too: under the user's cache, never `/tmp` (AGENTS.md, "Testing"); on
/// Windows, under their local application data. The unit tests' is the
/// crate's own (`Scratch` in lib.rs).
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(label: &str) -> Scratch {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        };
        let dir = base
            .unwrap()
            .join("skwird-tests")
            .join(format!("client-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
