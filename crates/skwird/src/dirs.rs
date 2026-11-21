//! Where a user's engine keeps what it keeps (the XDG base folders), and
//! where its socket is (`docs/DESIGN.md`, Protocol): every folder its user's
//! alone.

use std::path::{Path, PathBuf};

/// The longest path a Unix socket takes, its final NUL excluded: 108 bytes
/// on Linux, 104 on macOS.
#[cfg(target_os = "linux")]
pub const SOCKET_MAX: usize = 107;
#[cfg(not(target_os = "linux"))]
pub const SOCKET_MAX: usize = 103;

/// A user's engine's folders.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub home: PathBuf,
    /// `$XDG_RUNTIME_DIR/skwird`: the socket and the lock.
    pub runtime: PathBuf,
    /// `$XDG_CACHE_HOME/skwird`: the indexes, which are caches.
    pub cache: PathBuf,
    /// `$XDG_CONFIG_HOME/skwird`: the user's exclusions.
    pub config: PathBuf,
    /// `$XDG_STATE_HOME/skwird`: what clients registered, and the log of
    /// what the engine did.
    pub state: PathBuf,
}

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

impl Dirs {
    pub fn from_env() -> Result<Dirs, String> {
        let home = var("HOME").ok_or("HOME is not set")?;
        let runtime = var("XDG_RUNTIME_DIR")
            .ok_or("XDG_RUNTIME_DIR is not set: skwird keeps its socket there")?;
        Ok(Dirs {
            runtime: runtime.join("skwird"),
            cache: var("XDG_CACHE_HOME")
                .unwrap_or_else(|| home.join(".cache"))
                .join("skwird"),
            config: var("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("skwird"),
            state: var("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local/state"))
                .join("skwird"),
            home,
        })
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("socket")
    }

    pub fn lock(&self) -> PathBuf {
        self.runtime.join("lock")
    }
}

/// A socket's path, refused where it is longer than a Unix socket takes:
/// an error that says so, not the crash it was in fsearch.
pub fn check_socket_path(path: &Path) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len > SOCKET_MAX {
        return Err(format!(
            "the socket's path is {len} bytes, past the {SOCKET_MAX} a Unix socket's path may hold: \
             set XDG_RUNTIME_DIR to a shorter folder"
        ));
    }
    Ok(())
}

/// `dir`, made if it is not there, and its user's alone (mode 0700, owned
/// by the user running this): a folder someone else may write in, or
/// own, is refused, since a socket there could be anyone's.
#[cfg(unix)]
pub fn make_private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make a folder of skwird's: {e}"))?;
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot stat a folder of skwird's: {e}"))?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err("a folder of skwird's is not its user's own: refused".into());
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot make a folder of skwird's its user's alone: {e}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn make_private(_: &Path) -> Result<(), String> {
    Err(
        "skwird's engine runs on Linux so far: Windows comes with phase 4, macOS with phase 5"
            .into(),
    )
}
