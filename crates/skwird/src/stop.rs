//! `skwird stop` (`docs/DESIGN.md`, Protocol, `stop`): the user's engine
//! stopped where it runs this copy's program, and its end waited for. A
//! program that carries skwird runs it before its own files are replaced
//! (an installer's upgrade), and `logon remove --if-mine` before it takes
//! the logon task back.

use std::path::PathBuf;
use std::time::Duration;

use skwird_client::{Client, Error, Find, Stopped};

/// The longest the engine's end is waited for.
pub const WITHIN: Duration = Duration::from_secs(10);

/// What a stop found.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No engine of the user's answers.
    NoEngine,
    /// It ran this copy's program, and ended.
    Ended,
    /// It runs another copy's program, this one: left running.
    OtherCopy(PathBuf),
}

impl Outcome {
    /// What it tells its user.
    pub fn said(&self) -> String {
        match self {
            Outcome::NoEngine => "no engine of yours answers".into(),
            Outcome::Ended => "your engine stopped".into(),
            Outcome::OtherCopy(program) => format!(
                "your engine runs another copy of skwird, left running: {}",
                program.display()
            ),
        }
    }
}

/// Stop the user's engine where it runs a program in this one's folder,
/// and wait for its end. An error where it could not be asked, or had
/// not ended after [`WITHIN`]: this copy's engine may still run.
pub fn this_copys() -> Result<Outcome, String> {
    let dirs = crate::dirs::Dirs::from_env()?;
    let me = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let find = Find {
        // The user's engine alone, never the system service, and none
        // started.
        socket: Some(dirs.socket()),
        start: None,
        silence: Some(WITHIN),
        ..Find::new("skwird")
    };
    let client = match Client::connect(&find) {
        Ok(client) => client,
        Err(Error::NoEngine(_)) => return Ok(Outcome::NoEngine),
        Err(e) => return Err(format!("cannot reach your engine: {e}")),
    };
    let welcome = client.welcome();
    if !welcome.features.iter().any(|f| f == skwird_protocol::STOP) {
        return Err(format!(
            "your engine, {}, cannot be asked to stop: skwird 0.3 is the first that can",
            welcome.engine
        ));
    }
    match client.stop(Some(&me), WITHIN) {
        Ok(Stopped::Ended) => Ok(Outcome::Ended),
        Ok(Stopped::Ending) => Err(format!(
            "your engine was asked to stop, and had not ended after {} s",
            WITHIN.as_secs()
        )),
        Ok(Stopped::OtherCopy(program)) => Ok(Outcome::OtherCopy(program)),
        Err(e) => Err(format!("your engine could not be asked to stop: {e}")),
    }
}
