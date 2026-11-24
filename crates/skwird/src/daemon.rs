//! The system service on macOS (`docs/DESIGN.md`, decision 1;
//! `docs/PLAN.md`, phase 5, step 7): `skwird serve --system --forever`, a
//! `LaunchDaemon` (`dist/macos/org.skwir.skwird-system.plist`), started by
//! launchd at each boot, as root: macOS has no capabilities, and only root
//! reads every user's folders.
//!
//! - **Registered** by `sudo skwird service add`, which starts it, as
//!   `systemctl enable --now` does; `remove` stops it and takes it back.
//!   The roots given (`--root`) are its own; none, those it finds (`/` and
//!   the Data volume's firmlinks).
//! - **Its log** in `/Library/Logs/skwird`, where Console shows it: counts,
//!   sizes and times, never a name.
//! - **Full Disk Access** is the user's to give it, in System Settings:
//!   without it, the folders macOS asks consent for in each home are never
//!   opened, and not covered (DESIGN 4).

use std::path::Path;

use crate::logon::{escape, fill};

/// The `LaunchDaemon`'s label, a name of launchd's for it: the system
/// domain's, beside each user's agent (`org.skwir.skwird`).
pub const LABEL: &str = "org.skwir.skwird-system";

/// Where launchd reads it at boot.
pub const PLIST: &str = "/Library/LaunchDaemons/org.skwir.skwird-system.plist";

/// Where it logs.
pub const LOGS: &str = "/Library/Logs/skwird";

/// The daemon's definition, for `program` given `args` after its own
/// (`--root PATH`...), logging to `log`: the template's places filled,
/// each value escaped as XML takes it, each argument a string of its own.
pub fn definition(program: &Path, args: &[String], log: &Path) -> String {
    let mut arguments = String::new();
    for a in args {
        arguments.push_str("\n\t\t<string>");
        arguments.push_str(&escape(a));
        arguments.push_str("</string>");
    }
    fill(
        include_str!("../../../dist/macos/org.skwir.skwird-system.plist"),
        &[
            ("program", &escape(&program.to_string_lossy())),
            ("arguments", &arguments),
            ("log", &escape(&log.to_string_lossy())),
        ],
    )
}

/// Refused unless run as root: launchd's system domain is root's.
#[cfg(target_os = "macos")]
fn as_root(what: &str) -> Result<(), String> {
    if rustix::process::geteuid().is_root() {
        Ok(())
    } else {
        Err(format!("service {what} is root's: run it with sudo"))
    }
}

/// Register the system service and start it: its definition written,
/// its program this one, given `args`, and taken by launchd, which starts
/// it now as at each boot. One already taken is taken back first, so that
/// the new one is what runs.
#[cfg(target_os = "macos")]
pub fn add(args: &[String]) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    as_root("add")?;
    let me = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("cannot find this program: {e}"))?;
    let logs = Path::new(LOGS);
    std::fs::create_dir_all(logs).map_err(|e| format!("cannot make its log's folder: {e}"))?;
    std::fs::set_permissions(logs, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("cannot open its log's folder to be read: {e}"))?;
    std::fs::write(PLIST, definition(&me, args, &logs.join("engine.log")))
        .map_err(|e| format!("cannot write the LaunchDaemon: {e}"))?;
    // launchd refuses a definition that another may write.
    std::fs::set_permissions(PLIST, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("cannot make the LaunchDaemon root's alone to write: {e}"))?;
    let _ = crate::logon::launchctl(&["bootout", &format!("system/{LABEL}")]);
    crate::logon::launchctl(&["bootstrap", "system", PLIST])?;
    Ok(format!(
        "the system service runs, and starts at each boot: {PLIST}"
    ))
}

/// Stop the system service and take it back: launchd starts it at no boot
/// after.
#[cfg(target_os = "macos")]
pub fn remove() -> Result<String, String> {
    as_root("remove")?;
    let taken = crate::logon::launchctl(&["bootout", &format!("system/{LABEL}")]);
    let removed = std::fs::remove_file(PLIST);
    match (taken, removed) {
        (Err(e), Err(_)) => Err(format!("no LaunchDaemon of skwird's was taken: {e}")),
        _ => Ok("the system service is stopped, and starts at no boot".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition runs the program given, as the system service, for
    /// good, given each argument as a string of its own, logging where it
    /// is given, a value that XML would misread escaped; on macOS, its own
    /// tool reads it as a property list (`plutil -lint`).
    #[test]
    fn the_daemon_runs_the_system_service_at_boot() {
        let plist = definition(
            Path::new("/opt/R&D <tools>/skwird"),
            &["--root".into(), "/Users/Shared/a {log} & b".into()],
            Path::new("/Library/Logs/skwird/engine.log"),
        );
        assert!(
            plist.contains("<string>/opt/R&amp;D &lt;tools&gt;/skwird</string>"),
            "{plist}"
        );
        let flat = plist.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(
                "<string>serve</string> <string>--system</string> <!-- Always: idle never ends it. --> \
                 <string>--forever</string> <string>--root</string> \
                 <string>/Users/Shared/a {log} &amp; b</string> </array>"
            ),
            "{flat}"
        );
        assert!(flat.contains("<key>RunAtLoad</key> <true/>"), "{flat}");
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(plist.contains("<string>/Library/Logs/skwird/engine.log</string>"));
        assert!(
            !plist.contains("{program}") && !plist.contains("{arguments}"),
            "{plist}"
        );
        #[cfg(target_os = "macos")]
        {
            let s = skwird_testkit::Scratch::new("daemon");
            let file = s.write("daemon.plist", plist.as_bytes());
            let out = std::process::Command::new("plutil")
                .arg("-lint")
                .arg(&file)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }
}
