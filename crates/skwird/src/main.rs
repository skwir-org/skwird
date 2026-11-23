//! skwird: a user's engine, and the command line that asks it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use skwird_client::{Client, Find};
use skwird_protocol::{Coverage, Match, NameTerm, Proof, Query, WirePath};

const USAGE: &str = "\
usage: skwird serve [--system] [--root PATH]... [--idle SECONDS | --forever] [--threads N]
       skwird find TERM [--in PATH] [--limit N] [--case]
       skwird coverage [PATH]
       skwird sync [PATH]
       skwird watch [--remove] FOLDER...
       skwird status
       skwird logon add|remove
       skwird service add [--root PATH]... | remove

serve     the user's engine: it indexes the roots (the home by default),
          answers on its socket, and goes once its last client has been
          gone SECONDS (SKWIRD_IDLE, or 60), or never with --forever
find      the entries whose names hold TERM (a glob where it holds * ? or
          [), as skwir matches them, below PATH (the current folder)
coverage  how far answers below PATH can be trusted
sync      prove PATH current now, then say how far it can be trusted
watch     keep FOLDER live: changes in it heard and applied as they come,
          within the engine's share of watches (--remove: take it back)
status    what the engine says of itself
logon     on Windows, start the engine at your logon (add), or no more
          (remove): a task of yours, running skwirdw.exe, skwird with no
          console window
service   on Windows, the system service (add, as an administrator): every
          user's files indexed, each user answered what they could list,
          started at boot as LocalSystem; remove stops it and takes it back

The commands that ask start the engine where none answers, unless
SKWIRD_NO_START is set. SKWIRD_WATCHES sets the most folders an engine
watches (a quarter of the user's inotify watches by default).";

struct Args {
    command: String,
    roots: Vec<PathBuf>,
    idle: Option<Duration>,
    threads: usize,
    term: Option<String>,
    path: Option<PathBuf>,
    /// The folders `watch` names, and whether it takes them back.
    folders: Vec<PathBuf>,
    remove: bool,
    /// `serve --system`: the system service.
    system: bool,
    limit: Option<u64>,
    case: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut it = args.iter();
    let command = it.next().ok_or(USAGE)?.clone();
    let mut parsed = Args {
        command,
        roots: Vec::new(),
        // SKWIRD_IDLE: the seconds an engine started without --idle stays
        // once its last client left (60 by default): what a test starts
        // through a client goes soon.
        idle: Some(Duration::from_secs(
            std::env::var("SKWIRD_IDLE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        )),
        threads: std::thread::available_parallelism().map_or(4, std::num::NonZero::get),
        term: None,
        path: None,
        limit: None,
        case: false,
        folders: Vec::new(),
        remove: false,
        system: false,
    };
    let number = |v: Option<&String>, what: &str| -> Result<u64, String> {
        v.and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{what} takes a number"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--root" => parsed
                .roots
                .push(PathBuf::from(it.next().ok_or("--root takes a folder")?)),
            "--idle" => parsed.idle = Some(Duration::from_secs(number(it.next(), "--idle")?)),
            "--forever" => parsed.idle = None,
            "--threads" => parsed.threads = number(it.next(), "--threads")?.max(1) as usize,
            "--in" => parsed.path = Some(PathBuf::from(it.next().ok_or("--in takes a folder")?)),
            "--limit" => parsed.limit = Some(number(it.next(), "--limit")?),
            "--case" => parsed.case = true,
            "--remove" => parsed.remove = true,
            "--system" => parsed.system = true,
            "-h" | "--help" => return Err(USAGE.into()),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}\n\n{USAGE}"));
            }
            word if matches!(parsed.command.as_str(), "find" | "logon" | "service")
                && parsed.term.is_none() =>
            {
                parsed.term = Some(word.to_owned());
            }
            word if parsed.command == "watch" => parsed.folders.push(PathBuf::from(word)),
            word if parsed.path.is_none() => parsed.path = Some(PathBuf::from(word)),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

// Also `skwirdw.exe`'s, which takes this file as a module on Windows.
pub(crate) fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Start the engine at the user's logon, or no more (Windows).
#[cfg(windows)]
fn logon(what: Option<&str>) -> Result<(), String> {
    let said = match what {
        Some("add") => skwird::logon::add()?,
        Some("remove") => skwird::logon::remove()?,
        _ => return Err("logon takes add or remove".into()),
    };
    println!("{said}");
    Ok(())
}

/// Register the system service and start it, or stop it and take it back
/// (Windows): the roots named, if any, given to it.
#[cfg(windows)]
fn service(args: &Args) -> Result<(), String> {
    let said = match args.term.as_deref() {
        Some("add") => {
            let mut given = Vec::new();
            for root in &args.roots {
                let root = skwird_crawl::canonical(root)
                    .map_err(|e| format!("cannot reach a root: {e}"))?;
                given.push("--root".to_owned());
                given.push(root.to_string_lossy().into_owned());
            }
            skwird::service::add(&given)?
        }
        Some("remove") => skwird::service::remove()?,
        _ => return Err("service takes add or remove".into()),
    };
    println!("{said}");
    Ok(())
}

#[cfg(not(windows))]
fn service(_: &Args) -> Result<(), String> {
    Err(
        "service is Windows': elsewhere, the service manager's unit starts it \
         (dist/systemd/skwird-system.service)"
            .into(),
    )
}

#[cfg(not(windows))]
fn logon(_: Option<&str>) -> Result<(), String> {
    Err(
        "logon is Windows': elsewhere, a service manager starts the engine \
         (dist/systemd/skwird.service)"
            .into(),
    )
}

/// The folder a command asks about: as given, made absolute and resolved.
fn folder(args: &Args) -> Result<PathBuf, String> {
    let path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    skwird_crawl::canonical(&path).map_err(|e| format!("cannot reach the folder: {e}"))
}

fn client() -> Result<Client, String> {
    let mut find = Find::new("skwird");
    // This program is the engine: start it where none answers, unless
    // SKWIRD_NO_START says not to (a measurement that runs its own).
    find.start = std::env::var_os("SKWIRD_NO_START")
        .is_none()
        .then(|| std::env::current_exe().ok())
        .flatten();
    Client::connect(&find).map_err(|e| e.to_string())
}

fn said(proof: Proof) -> String {
    let at = |t: skwird_protocol::Instant| format!("{}.{:09}", t.sec, t.nsec);
    match proof {
        Proof::Live(t) => format!("live since {}", at(t)),
        Proof::Checked(t) => format!("checked at {}", at(t)),
        Proof::AsOf(t) => format!("as of {}", at(t)),
        Proof::NotCovered(why) => format!("not covered: {why:?}"),
    }
}

fn print_coverage(c: &Coverage) {
    println!("names: {}", said(c.names));
    println!("attributes: {}", said(c.attrs));
    for hole in &c.holes {
        println!(
            "not covered: {} ({:?})",
            hole.path.to_path().display(),
            hole.why
        );
    }
}

fn run(args: &Args) -> Result<(), String> {
    match args.command.as_str() {
        "serve" => serve(args),
        "logon" => logon(args.term.as_deref()),
        "service" => service(args),
        "find" => {
            let term = args.term.clone().ok_or("find takes a term")?;
            let root = folder(args)?;
            let mut c = client()?;
            let query = Query {
                root: WirePath::of(&root),
                name: Some(NameTerm {
                    how: Match::Skwir,
                    text: term,
                    case_sensitive: args.case,
                }),
                kinds: None,
                size: None,
                mtime: None,
                limit: args.limit,
                links: skwird_protocol::Links::Own,
            };
            let done = c
                .query(&query, |batch| {
                    for e in batch {
                        println!("{}", e.path.to_path().display());
                    }
                    true
                })
                .map_err(|e| e.to_string())?;
            eprintln!(
                "{} found{}; names {}",
                done.count,
                if done.truncated {
                    ", more past the limit"
                } else {
                    ""
                },
                said(done.coverage.names)
            );
            Ok(())
        }
        "coverage" => {
            let c = client()?
                .coverage(&folder(args)?)
                .map_err(|e| e.to_string())?;
            print_coverage(&c);
            Ok(())
        }
        "sync" => {
            let c = client()?.sync(&folder(args)?).map_err(|e| e.to_string())?;
            print_coverage(&c);
            Ok(())
        }
        "watch" => watch(args),
        "status" => {
            let c = client()?;
            let w = c.welcome();
            println!("{} (protocol {}, {:?} scope)", w.engine, w.version, w.scope);
            for root in &w.roots {
                println!("root: {}", root.to_path().display());
            }
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(any(target_os = "linux", windows))]
fn serve(args: &Args) -> Result<(), String> {
    // On Windows a client starts the engine with none of its handles, and
    // names its log (`SKWIRD_LOG`): what it logs goes there.
    #[cfg(windows)]
    if let Some(log) = std::env::var_os("SKWIRD_LOG") {
        let _ = skwird::os::log_to(std::path::Path::new(&log));
    }
    // The system service's folders are its service manager's; its roots,
    // unless named, every local filesystem that stores files (DESIGN 1):
    // on Windows, every volume of the machine's own that does.
    let dirs = if args.system {
        skwird::dirs::Dirs::system()
    } else {
        skwird::dirs::Dirs::from_env()?
    };
    let roots = match (args.roots.is_empty(), args.system) {
        (false, _) => args.roots.clone(),
        (true, false) => vec![dirs.home.clone()],
        #[cfg(windows)]
        (true, true) => skwird::system::volume_roots(),
        #[cfg(not(windows))]
        (true, true) => {
            let table = skwird_crawl::mounts::Table::read()
                .map_err(|e| format!("cannot read the mount table: {e}"))?;
            skwird::system::roots(&table, skwird::system::removable)
        }
    };
    // Named as clients name them (on Windows, plainly).
    let roots = roots
        .iter()
        .map(|r| skwird_crawl::canonical(r).map_err(|e| format!("cannot reach a root: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let config = skwird::engine::Config {
        dirs,
        roots,
        idle: args.idle,
        threads: args.threads,
        // SKWIRD_WATCHES: the most folders it watches, the user's share by
        // default: a test gives fewer.
        watches: std::env::var("SKWIRD_WATCHES")
            .ok()
            .and_then(|v| v.parse().ok()),
        system: args.system,
        discovered: args.system && args.roots.is_empty(),
    };
    // On Windows, the system service as the service manager starts it; or
    // in the foreground, run from a console.
    #[cfg(windows)]
    if args.system && skwird::service::dispatch(&config)? {
        return Ok(());
    }
    skwird::serve::serve(&config)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn serve(args: &Args) -> Result<(), String> {
    skwird::serve(&args.roots, args.idle)
}

/// Keep the folders named live, or take them back; then say how far
/// answers below each can be trusted.
fn watch(args: &Args) -> Result<(), String> {
    if args.folders.is_empty() {
        return Err("watch takes a folder".into());
    }
    let folders = args
        .folders
        .iter()
        .map(|f| skwird_crawl::canonical(f).map_err(|e| format!("cannot reach a folder: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let named: Vec<&Path> = folders.iter().map(PathBuf::as_path).collect();
    let mut c = client()?;
    let (add, remove): (&[&Path], &[&Path]) = if args.remove {
        (&[], &named)
    } else {
        (&named, &[])
    };
    let watching = c.watch(add, remove).map_err(|e| e.to_string())?;
    for kept in &watching.folders {
        let state = match kept.state {
            skwird_protocol::Keeping::Live(since) => said(Proof::Live(since)),
            skwird_protocol::Keeping::NotLive(why) => format!("not live: {why:?}"),
        };
        let folders = kept
            .folders
            .map(|n| format!(", {n} folders"))
            .unwrap_or_default();
        let within = kept
            .within
            .as_ref()
            .map(|w| format!(", within {}", w.to_path().display()))
            .unwrap_or_default();
        println!(
            "{}: {state}{folders}{within}",
            kept.path.to_path().display()
        );
    }
    if let Some(share) = watching.share {
        println!("watches: {} of {}", share.used, share.size);
    }
    Ok(())
}
