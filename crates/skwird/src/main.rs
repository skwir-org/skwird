//! skwird: a user's engine, and the command line that asks it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use skwird_client::{Client, Find};
use skwird_protocol::{Coverage, Match, NameTerm, Proof, Query, WirePath};

const USAGE: &str = "\
usage: skwird serve [--root PATH]... [--idle SECONDS | --forever] [--threads N]
       skwird find TERM [--in PATH] [--limit N] [--case]
       skwird coverage [PATH]
       skwird sync [PATH]
       skwird status

serve     the user's engine: it indexes the roots (the home by default),
          answers on its socket, and goes once its last client has been
          gone SECONDS (SKWIRD_IDLE, or 60), or never with --forever
find      the entries whose names hold TERM (a glob where it holds * ? or
          [), as skwir matches them, below PATH (the current folder)
coverage  how far answers below PATH can be trusted
sync      prove PATH current now, then say how far it can be trusted
status    what the engine says of itself

The commands that ask start the engine where none answers, unless
SKWIRD_NO_START is set.";

struct Args {
    command: String,
    roots: Vec<PathBuf>,
    idle: Option<Duration>,
    threads: usize,
    term: Option<String>,
    path: Option<PathBuf>,
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
            "-h" | "--help" => return Err(USAGE.into()),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}\n\n{USAGE}"));
            }
            word if parsed.command == "find" && parsed.term.is_none() => {
                parsed.term = Some(word.to_owned());
            }
            word if parsed.path.is_none() => parsed.path = Some(PathBuf::from(word)),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The folder a command asks about: as given, made absolute and resolved.
fn folder(args: &Args) -> Result<PathBuf, String> {
    let path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    std::fs::canonicalize(&path).map_err(|e| format!("cannot reach the folder: {e}"))
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

#[cfg(target_os = "linux")]
fn serve(args: &Args) -> Result<(), String> {
    let dirs = skwird::dirs::Dirs::from_env()?;
    let roots = if args.roots.is_empty() {
        vec![dirs.home.clone()]
    } else {
        args.roots.clone()
    };
    let roots = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).map_err(|e| format!("cannot reach a root: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    skwird::serve::serve(&skwird::engine::Config {
        dirs,
        roots,
        idle: args.idle,
        threads: args.threads,
    })
}

#[cfg(not(target_os = "linux"))]
fn serve(args: &Args) -> Result<(), String> {
    skwird::serve(&args.roots, args.idle)
}
