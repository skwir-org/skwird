//! skwird-bench: what an index of names costs on a real tree, measured
//! before skwird builds one (docs/PLAN.md, phase 0). It prints counts,
//! sizes and times, never a name or a path, so what it prints can be
//! shared.

use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(unix)]
mod check;
#[cfg(target_os = "linux")]
mod fanotify;
mod said;
#[cfg(unix)]
mod walk;

const USAGE: &str = "\
usage: skwird-bench walk ROOT [--threads N] [--stat] [--cross-mounts]
       skwird-bench check ROOT [--threads N] [--runs N] [--cold] [--drop-with CMD]
       skwird-bench fanotify FOLDER [--seconds N] [--probe DIR]...

walk      read every folder under ROOT as skwird's crawl would, and size
          the index it would keep (--stat: stat every entry, as an index
          of sizes and times needs)
check     what proving that index current costs where nothing watched:
          every folder's change time, on 1, 8 and all threads (--cold:
          asks you to drop the caches before each cold pass; --drop-with
          CMD: runs CMD with sh to drop them instead, and implies --cold)
fanotify  whether a system service could watch the whole filesystem
          holding FOLDER (root only). Refused in a btrfs subvolume, it
          tries through the filesystem's top-level subvolume, mounted
          read-only and attached nowhere while it runs. --seconds N:
          listen that long and say which filesystems the events came
          from; --probe DIR: first make, rename and remove a file of its
          own in DIR, and say whether each of those was heard and
          whether its folder's handle opens

It prints counts, sizes and times, never a name or a path.";

struct Args {
    command: String,
    path: PathBuf,
    threads: usize,
    runs: usize,
    stat: bool,
    cross_mounts: bool,
    cold: bool,
    /// A command that drops the caches, run with `sh -c` before each cold pass.
    drop_with: Option<String>,
    seconds: u64,
    /// Folders where `fanotify` makes, renames and removes a file of its own.
    probes: Vec<PathBuf>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut it = args.iter();
    let command = it.next().ok_or(USAGE)?.clone();
    let mut parsed = Args {
        command,
        path: PathBuf::new(),
        threads: std::thread::available_parallelism().map_or(8, std::num::NonZero::get),
        runs: 3,
        stat: false,
        cross_mounts: false,
        cold: false,
        drop_with: None,
        seconds: 0,
        probes: Vec::new(),
    };
    let number = |v: Option<&String>, what: &str| -> Result<u64, String> {
        v.and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{what} takes a number"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--threads" => parsed.threads = number(it.next(), "--threads")?.max(1) as usize,
            "--runs" => parsed.runs = number(it.next(), "--runs")?.max(1) as usize,
            "--seconds" => parsed.seconds = number(it.next(), "--seconds")?,
            "--stat" => parsed.stat = true,
            "--cross-mounts" => parsed.cross_mounts = true,
            "--cold" => parsed.cold = true,
            "--drop-with" => {
                parsed.drop_with = Some(it.next().ok_or("--drop-with takes a command")?.clone());
                parsed.cold = true;
            }
            "--probe" => {
                let dir = it.next().ok_or("--probe takes a folder")?;
                parsed.probes.push(
                    std::fs::canonicalize(dir).map_err(|e| format!("cannot reach a probe: {e}"))?,
                );
            }
            "-h" | "--help" => return Err(USAGE.into()),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}\n\n{USAGE}"));
            }
            path if parsed.path.as_os_str().is_empty() => parsed.path = PathBuf::from(path),
            _ => return Err(USAGE.into()),
        }
    }
    if parsed.path.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    // The walk opens folders without following links: a root given through
    // one is walked where it leads.
    parsed.path =
        std::fs::canonicalize(&parsed.path).map_err(|e| format!("cannot reach the path: {e}"))?;
    Ok(parsed)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args).and_then(|args| run(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn run(_: &Args) -> Result<(), String> {
    Err("skwird-bench measures Unix filesystems so far; Windows comes with phase 4".into())
}

#[cfg(unix)]
fn run(args: &Args) -> Result<(), String> {
    match args.command.as_str() {
        "walk" => {
            let walked = walk::walk(&args.path, &walk_options(args, args.stat))?;
            print_walk(&walked, args);
            Ok(())
        }
        "check" => check(args),
        #[cfg(target_os = "linux")]
        "fanotify" => fanotify::probe(&args.path, args.seconds, &args.probes),
        #[cfg(not(target_os = "linux"))]
        "fanotify" => Err("fanotify is Linux's".into()),
        _ => Err(USAGE.into()),
    }
}

#[cfg(unix)]
fn walk_options(args: &Args, stat_files: bool) -> walk::Options {
    walk::Options {
        threads: args.threads,
        stat_files,
        cross_mounts: args.cross_mounts,
    }
}

#[cfg(unix)]
fn print_walk(walked: &walk::Walked, args: &Args) {
    use said::{bytes, count};
    println!("filesystem: {}", walk::fs_name(walked.fs_type));
    println!(
        "entries: {} ({} folders, {} files, {} links, {} other)",
        count(walked.entries()),
        count(walked.folders),
        count(walked.files),
        count(walked.links),
        count(walked.others)
    );
    println!(
        "names: {} in all, {} distinct ({})",
        bytes(walked.name_bytes),
        count(walked.distinct),
        bytes(walked.distinct_bytes)
    );
    println!(
        "not walked: {} mounts below the root{}, {} folders unreadable",
        count(walked.mounts),
        if args.cross_mounts {
            ""
        } else {
            " (--cross-mounts walks them)"
        },
        count(walked.unreadable)
    );
    if walked.again > 0 {
        println!(
            "walked once: {} folders reached again through a bind mount",
            count(walked.again)
        );
    }
    println!(
        "walk: {} on {} threads, {}",
        said::time(walked.took),
        args.threads,
        if args.stat {
            "every entry stat-ed"
        } else {
            "folders stat-ed, files named only"
        }
    );
    #[cfg(target_os = "linux")]
    if let Some(limit) = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        println!(
            "inotify: a watch per folder would take {} of this user's {}",
            count(walked.folders),
            count(limit)
        );
    }
    let (entries, names, folders) = estimate(walked);
    println!(
        "estimated index: {} (entries {}, names {}, folders {})",
        bytes(entries + names + folders),
        bytes(entries),
        bytes(names),
        bytes(folders)
    );
}

/// The index skwird would keep for what was walked, in fsearch's layout
/// with what skwird adds (docs/DESIGN.md, "The index"): per entry its
/// name's number, parent, kind, size, modification time and inode (29
/// bytes) and its place in its name's list of entries (4); per distinct
/// name its bytes, their offset and its letter mask (12) and its list's
/// offset (4); per folder its range, parent, change time, owner, group
/// and mode (40). An estimate, until the index is built.
#[cfg(unix)]
fn estimate(walked: &walk::Walked) -> (u64, u64, u64) {
    (
        walked.entries() * (29 + 4),
        walked.distinct * (12 + 4) + walked.distinct_bytes,
        walked.folders * 40,
    )
}

#[cfg(unix)]
fn check(args: &Args) -> Result<(), String> {
    let walked = walk::walk(&args.path, &walk_options(args, false))?;
    println!(
        "walk: {} folders in {} on {} threads ({} not walked: mounts and unreadable)",
        said::count(walked.list.len() as u64),
        said::time(walked.took),
        args.threads,
        said::count(walked.mounts + walked.unreadable)
    );
    let groups = check::Groups::of(&walked.list);
    let mut threads = vec![1, 8.min(args.threads), args.threads];
    threads.dedup();
    println!("pass       threads  by path     through parents  changed  gone");
    let row = |pass: &str, n: usize, path: &check::Checked, parent: &check::Checked| {
        println!(
            "{pass:<10} {n:>7}  {:>9}   {:>15}  {:>7}  {:>4}",
            said::time(path.took),
            said::time(parent.took),
            said::count(path.changed),
            said::count(path.gone)
        );
    };
    if args.cold {
        let drop_caches = || -> Result<(), String> {
            if let Some(command) = &args.drop_with {
                let ran = std::process::Command::new("sh")
                    .args(["-c", command])
                    .status()
                    .map_err(|e| format!("cannot run the command that drops the caches: {e}"))?;
                return if ran.success() {
                    Ok(())
                } else {
                    Err(format!("the command that drops the caches failed ({ran})"))
                };
            }
            println!(
                "  drop the caches now (sync; echo 3 | sudo tee /proc/sys/vm/drop_caches), then press Enter"
            );
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            Ok(())
        };
        // A cache that could not be dropped ends the cold passes, not the
        // check: the warm ones still run.
        let cold = |n: usize| -> Result<(check::Checked, check::Checked), String> {
            drop_caches()?;
            let path = check::by_path(&walked.list, n);
            drop_caches()?;
            Ok((path, check::by_parent(&groups, n)))
        };
        for &n in &threads {
            match cold(n) {
                Ok((path, parent)) => row("cold", n, &path, &parent),
                Err(why) => {
                    println!("cold passes skipped: {why}");
                    break;
                }
            }
        }
    }
    for run in 1..=args.runs {
        for &n in &threads {
            let path = check::by_path(&walked.list, n);
            let parent = check::by_parent(&groups, n);
            row(&format!("warm {run}"), n, &path, &parent);
        }
    }
    Ok(())
}
