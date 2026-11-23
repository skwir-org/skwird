//! The user's engine for the session on Windows (`docs/DESIGN.md`,
//! decision 1; `docs/PLAN.md`, phase 4, step 6): a task of the user's,
//! started at their logon (`dist/windows/skwird-logon.xml`), registered
//! with Task Scheduler (`schtasks`). It runs `skwirdw.exe`, skwird with no
//! console window, which a task would otherwise open; with `--forever`,
//! as the systemd user unit does on Linux.

use std::path::Path;

/// The task's definition, for `program` run as `user` (a SID): the
/// template's places filled, each value escaped as XML takes it.
pub fn task(program: &Path, user: &str) -> String {
    include_str!("../../../dist/windows/skwird-logon.xml")
        .replace("{program}", &escape(&program.to_string_lossy()))
        .replace("{user}", &escape(user))
}

/// The task's name, the user's own: a name is the machine's, every
/// user's tasks among the same.
pub fn name(user: &str) -> String {
    format!("skwird-{user}")
}

fn escape(text: &str) -> String {
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
}
