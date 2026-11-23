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
    /// On Windows, the pipe it answers on, a name and no file: its user's
    /// (`\\.\pipe\skwird-<SID>`), or the system's (`\\.\pipe\skwird`), or
    /// the one `SKWIRD_PIPE` names (a test's own). None where its user's
    /// SID cannot be read.
    #[cfg(windows)]
    pub pipe: Option<PathBuf>,
}

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

impl Dirs {
    /// A user's engine's folders on Windows (`docs/PLAN.md`, phase 4,
    /// step 4): in the user's local application data, never roamed,
    /// `skwird`'s own; the home its profile. Its pipe is its user's own.
    #[cfg(windows)]
    pub fn from_env() -> Result<Dirs, String> {
        let home = var("USERPROFILE").ok_or("USERPROFILE is not set")?;
        let base = var("LOCALAPPDATA")
            .ok_or("LOCALAPPDATA is not set: skwird keeps its index there")?
            .join("skwird");
        Ok(Dirs {
            runtime: base.join("run"),
            cache: base.join("cache"),
            config: base.join("config"),
            state: base.join("state"),
            home,
            pipe: skwird_client::pipe::user_pipe(),
        })
    }

    #[cfg(not(windows))]
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

    /// The system service's folders (`docs/DESIGN.md`, Protocol): as its
    /// service manager names them (systemd's `RUNTIME_DIRECTORY`,
    /// `CACHE_DIRECTORY`, `STATE_DIRECTORY` and `CONFIGURATION_DIRECTORY`,
    /// the first of each list), or the usual places.
    #[cfg(not(windows))]
    pub fn system() -> Dirs {
        let dir = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.split(':').next().map(PathBuf::from))
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| PathBuf::from(default))
        };
        Dirs {
            home: PathBuf::from("/root"),
            runtime: dir("RUNTIME_DIRECTORY", "/run/skwird"),
            cache: dir("CACHE_DIRECTORY", "/var/cache/skwird"),
            state: dir("STATE_DIRECTORY", "/var/lib/skwird"),
            config: dir("CONFIGURATION_DIRECTORY", "/etc/skwird"),
        }
    }

    /// The system service's folders on Windows (`docs/PLAN.md`, phase 4,
    /// step 7): in the machine's program data (`%ProgramData%\skwird`, a
    /// test's own where it names another), its own alone
    /// ([`make_system`]). Its pipe is the system's, `\\.\pipe\skwird`, or
    /// the one `SKWIRD_PIPE` names. It has no home: the folder is its own.
    #[cfg(windows)]
    pub fn system() -> Dirs {
        let base = var("ProgramData")
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("skwird");
        let pipe = std::env::var_os("SKWIRD_PIPE")
            .filter(|v| !v.is_empty())
            .map_or_else(skwird_client::system_socket, PathBuf::from);
        Dirs {
            runtime: base.join("run"),
            cache: base.join("cache"),
            config: base.join("config"),
            state: base.join("state"),
            home: base,
            pipe: Some(pipe),
        }
    }

    /// Where the engine answers: its socket in its runtime folder; on
    /// Windows, its pipe.
    pub fn socket(&self) -> PathBuf {
        #[cfg(windows)]
        if let Some(pipe) = &self.pipe {
            return pipe.clone();
        }
        self.runtime.join("socket")
    }

    pub fn lock(&self) -> PathBuf {
        self.runtime.join("lock")
    }
}

/// A pipe's name: no limit as a Unix socket's path has.
#[cfg(windows)]
// The same call as on Unix, which may refuse.
#[allow(clippy::unnecessary_wraps)]
pub fn check_socket_path(_: &Path) -> Result<(), String> {
    Ok(())
}

/// A socket's path, refused where it is longer than a Unix socket takes:
/// an error that says so, not the crash it was in fsearch.
#[cfg(not(windows))]
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
    // The folder a link there leads to: with DynamicUser, systemd makes
    // the service's state and cache folders links into `private/`. It
    // must still be the user's own, as checked here.
    let meta =
        std::fs::metadata(dir).map_err(|e| format!("cannot stat a folder of skwird's: {e}"))?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err("a folder of skwird's is not its user's own: refused".into());
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot make a folder of skwird's its user's alone: {e}"))?;
    }
    Ok(())
}

/// `dir`, made if it is not there. On Windows, a folder in the user's
/// local application data inherits its profile's access, its user's
/// alone; and no socket is in it, the pipe having a security descriptor
/// of its own.
#[cfg(windows)]
pub fn make_private(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make a folder of skwird's: {e}"))
}

/// The system service's folders made, before anything is kept in them.
/// On Windows, its folder in the machine's program data, and each of its
/// own in it, its own and the administrators' alone
/// (`crate::service::make_own`): no socket is in them, the pipe having a
/// security descriptor of its own.
#[cfg(windows)]
pub fn make_system(dirs: &Dirs) -> Result<(), String> {
    if let Some(base) = dirs.runtime.parent() {
        crate::service::make_own(base)?;
    }
    for dir in [&dirs.runtime, &dirs.cache, &dirs.config, &dirs.state] {
        crate::service::make_own(dir)?;
    }
    Ok(())
}

/// The system service's folders made, before anything is kept in them:
/// its runtime folder open to every user, who finds its socket there; its
/// state its own.
#[cfg(unix)]
pub fn make_system(dirs: &Dirs) -> Result<(), String> {
    make_shared(&dirs.runtime)?;
    make_private(&dirs.state)
}

/// `dir`, made if it is not there, owned by the user running this and
/// writable by it alone, but open to every user (mode 0755): the system
/// service's runtime folder, where every user finds its socket.
#[cfg(unix)]
pub fn make_shared(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make a folder of skwird's: {e}"))?;
    // The folder a link there leads to: with DynamicUser, systemd makes
    // the service's state and cache folders links into `private/`. It
    // must still be the user's own, as checked here.
    let meta =
        std::fs::metadata(dir).map_err(|e| format!("cannot stat a folder of skwird's: {e}"))?;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err("a folder of skwird's is not its user's own: refused".into());
    }
    if meta.mode() & 0o7777 != 0o755 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot open a folder of skwird's to every user: {e}"))?;
    }
    Ok(())
}
