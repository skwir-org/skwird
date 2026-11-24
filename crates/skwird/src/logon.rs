//! The user's engine for the session (`docs/DESIGN.md`, decision 1), with
//! `--forever`, as the systemd user unit runs it on Linux:
//! - on Windows (`docs/PLAN.md`, phase 4, step 6), a task of the user's,
//!   started at their logon (`dist/windows/skwird-logon.xml`), registered
//!   with Task Scheduler (`schtasks`). It runs `skwirdw.exe`, skwird with
//!   no console window, which a task would otherwise open;
//! - on macOS (phase 5, step 6), a `LaunchAgent` of the user's, started at
//!   their login (`dist/macos/org.skwir.skwird.plist`), taken by launchd
//!   (`launchctl bootstrap`).

use std::path::Path;

/// The task's definition, for `program` run as `user` (a SID): the
/// template's places filled, each value escaped as XML takes it.
pub fn task(program: &Path, user: &str) -> String {
    fill(
        include_str!("../../../dist/windows/skwird-logon.xml"),
        &[
            ("program", &escape(&program.to_string_lossy())),
            ("user", &escape(user)),
        ],
    )
}

/// The task's name, the user's own: a name is the machine's, every
/// user's tasks among the same.
pub fn name(user: &str) -> String {
    format!("skwird-{user}")
}

/// The `LaunchAgent`'s label, a name of launchd's for it (macOS).
pub const AGENT: &str = "org.skwir.skwird";

/// The `LaunchAgent`'s definition (macOS), for `program`, logging to `log`:
/// the template's places filled, each value escaped as XML takes it.
pub fn agent(program: &Path, log: &Path) -> String {
    fill(
        include_str!("../../../dist/macos/org.skwir.skwird.plist"),
        &[
            ("program", &escape(&program.to_string_lossy())),
            ("log", &escape(&log.to_string_lossy())),
        ],
    )
}

/// `template` with each place (`{name}`) of `places` filled, in one pass:
/// what a value holds is never read as a place, a path with `{log}` in it
/// among them. A place no value is given for is left as it is.
pub(crate) fn fill(template: &str, places: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            places
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v, close))
        });
        if let Some((v, close)) = value {
            out.push_str(v);
            rest = &after[close + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `text` as XML takes it in an element or an attribute.
pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The definition in UTF-16, its byte order mark first, as its
/// declaration says and Task Scheduler reads it.
pub fn utf16(xml: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

/// Register the task for the user running this: its program the
/// `skwirdw.exe` beside this program, its definition written to the
/// engine's state folder. Says what `schtasks` said.
#[cfg(windows)]
pub fn add() -> Result<String, String> {
    let me = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let program = me.with_file_name("skwirdw.exe");
    if !program.is_file() {
        return Err(format!(
            "no skwirdw.exe beside this program, which the task runs: {}",
            program.display()
        ));
    }
    let user = skwird_client::pipe::user_sid().map_err(|e| format!("cannot read your SID: {e}"))?;
    let dirs = crate::dirs::Dirs::from_env()?;
    crate::dirs::make_private(&dirs.state)?;
    let file = dirs.state.join("logon.xml");
    std::fs::write(&file, utf16(&task(&program, &user)))
        .map_err(|e| format!("cannot write the task's definition: {e}"))?;
    schtasks(&[
        "/Create",
        "/TN",
        &name(&user),
        "/XML",
        &file.to_string_lossy(),
        "/F",
    ])
}

/// Take back the task of the user running this.
#[cfg(windows)]
pub fn remove() -> Result<String, String> {
    let user = skwird_client::pipe::user_sid().map_err(|e| format!("cannot read your SID: {e}"))?;
    schtasks(&["/Delete", "/TN", &name(&user), "/F"])
}

/// What `schtasks` said, run with `args`; its error where it failed.
#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run schtasks: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(said)
    } else {
        Err(format!(
            "schtasks failed: {said} {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Where the user's `LaunchAgent` is, below their home (macOS).
#[cfg(target_os = "macos")]
fn agent_file(dirs: &crate::dirs::Dirs) -> std::path::PathBuf {
    dirs.home
        .join("Library/LaunchAgents")
        .join(format!("{AGENT}.plist"))
}

/// launchd's domain of the user running this, their login session's.
#[cfg(target_os = "macos")]
fn domain() -> String {
    format!("gui/{}", rustix::process::getuid().as_raw())
}

/// Have the engine started at the user's login (macOS): its `LaunchAgent`
/// written, its program this one, its log in the engine's state folder,
/// and taken by launchd, which starts it now as at each login. One
/// already taken is taken back first, so that the new one is what runs.
#[cfg(target_os = "macos")]
pub fn add() -> Result<String, String> {
    let me = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("cannot find this program: {e}"))?;
    let dirs = crate::dirs::Dirs::from_env()?;
    crate::dirs::make_private(&dirs.state)?;
    let file = agent_file(&dirs);
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)
            .map_err(|e| format!("cannot make the LaunchAgents folder: {e}"))?;
    }
    std::fs::write(&file, agent(&me, &dirs.state.join("engine.log")))
        .map_err(|e| format!("cannot write the LaunchAgent: {e}"))?;
    let _ = launchctl(&["bootout", &format!("{}/{AGENT}", domain())]);
    launchctl(&["bootstrap", &domain(), &file.to_string_lossy()])?;
    Ok(format!(
        "the engine starts at your login: {}",
        file.display()
    ))
}

/// Take back the user's `LaunchAgent` (macOS): launchd stops the engine it
/// started, and starts none at their next login.
#[cfg(target_os = "macos")]
pub fn remove() -> Result<String, String> {
    let dirs = crate::dirs::Dirs::from_env()?;
    let file = agent_file(&dirs);
    let taken = launchctl(&["bootout", &format!("{}/{AGENT}", domain())]);
    let removed = std::fs::remove_file(&file);
    match (taken, removed) {
        (Err(e), Err(_)) => Err(format!("no LaunchAgent of skwird's was taken: {e}")),
        _ => Ok("the engine starts at your login no more".into()),
    }
}

/// What `launchctl` said, run with `args`; its error where it failed.
#[cfg(target_os = "macos")]
pub(crate) fn launchctl(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run launchctl: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(said)
    } else {
        Err(format!(
            "launchctl {} failed: {said} {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition runs the program given, as the user given, at their
    /// logon, with no limit, a value that XML would misread escaped.
    #[test]
    fn the_task_runs_the_engine_at_the_users_logon() {
        let user = "S-1-5-21-1-2-3-1001";
        let xml = task(Path::new(r"C:\Programs\R&D <tools>\skwirdw.exe"), user);
        assert!(
            xml.contains(r#"<Command>"C:\Programs\R&amp;D &lt;tools&gt;\skwirdw.exe"</Command>"#),
            "{xml}"
        );
        assert!(xml.contains("<Arguments>serve --forever</Arguments>"));
        assert_eq!(xml.matches(&format!("<UserId>{user}</UserId>")).count(), 2);
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(!xml.contains('{'), "a place left unfilled: {xml}");
        assert_eq!(name(user), "skwird-S-1-5-21-1-2-3-1001");
        let bytes = utf16(&xml);
        assert_eq!(&bytes[..4], &[0xFF, 0xFE, b'<', 0]);
    }

    /// The `LaunchAgent` runs the program given, for the session (`--forever`),
    /// at the user's login, logging where it is given, a value that XML
    /// would misread escaped; on macOS, its own tool reads it as a property
    /// list (`plutil -lint`).
    #[test]
    fn the_agent_runs_the_engine_at_the_users_login() {
        let plist = agent(
            Path::new("/Applications/R&D <tools>/skwird"),
            Path::new("/Users/someone/.local/state/skwird/engine.log"),
        );
        assert!(
            plist.contains("<string>/Applications/R&amp;D &lt;tools&gt;/skwird</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>--forever</string>"));
        // Its spacing aside: a checkout on Windows ends its lines with CRLF.
        let flat = plist.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("<key>RunAtLoad</key> <true/>"), "{flat}");
        assert!(plist.contains(&format!("<string>{AGENT}</string>")));
        assert!(plist.contains("<string>/Users/someone/.local/state/skwird/engine.log</string>"));
        assert!(
            !plist.contains("{program}") && !plist.contains("{log}"),
            "{plist}"
        );
        #[cfg(target_os = "macos")]
        {
            let s = skwird_testkit::Scratch::new("agent");
            let file = s.write("agent.plist", plist.as_bytes());
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
