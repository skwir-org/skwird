//! What a busy time makes of a watched folder (`docs/PLAN.md`, phase 2,
//! step 2). The folder is watched as the engine is to watch it, a command
//! is run (a build, say), and what applying its changes costs is counted:
//! - at once, each folder an event names read again as soon as it is
//!   heard, its new folders crawled and watched, as the engine would;
//! - in batches, from the same reads: a folder's changes logged once its
//!   batch closes, after a time of quiet or at most a time after its
//!   first change, as fsearch 0.2.2 applies busy folders' changes.
//!
//! Each change is sized as the engine's log would hold it
//! (`skwird_index::log::size_of`), so the log's growth is known against
//! the 16 MB its journal keeps. It prints counts, sizes and times, never a
//! name or a path.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use skwird_crawl::{
    Cancel, Entry, Exclusions, FileId, Heard, Kind, Options, Outcome, Root, Time, Watch, Watcher,
    crawl_below, read_folder_watched,
};
use skwird_index::block::{Facts, State};
use skwird_index::build::facts;
use skwird_index::log::{Change, FolderRecord, Op, Record, size_of};

use crate::said::{bytes, count, time};

type Entries = BTreeMap<Vec<u8>, Entry>;

/// What applying changes cost.
#[derive(Default)]
struct Cost {
    reads: u64,
    records: u64,
    changes: u64,
    bytes: u64,
}

impl Cost {
    fn log(&mut self, path: &Path, facts: Option<Facts>, changes: Vec<Change>) {
        self.records += 1;
        self.changes += changes.len() as u64;
        self.bytes += size_of(&Record::Folder(FolderRecord {
            path: path.to_path_buf(),
            facts,
            state: State::Listed { again: false },
            changes,
        })) as u64;
    }
}

fn component(name: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(name))
}

/// What changed from `old` to `new`.
fn diff(old: &Entries, new: &Entries) -> Vec<Change> {
    let mut changes = Vec::new();
    for (name, entry) in new {
        match old.get(name) {
            None => changes.push(Change {
                op: Op::Added,
                name: name.clone(),
                entry: *entry,
            }),
            Some(was) if was != entry => changes.push(Change {
                op: Op::Changed,
                name: name.clone(),
                entry: *entry,
            }),
            Some(_) => {}
        }
    }
    for (name, entry) in old {
        if !new.contains_key(name) {
            changes.push(Change {
                op: Op::Removed,
                name: name.clone(),
                entry: *entry,
            });
        }
    }
    changes
}

/// Whether `name` is a folder in `new` that `old` did not hold as the
/// same folder.
fn is_new_folder(old: &Entries, name: &[u8], entry: &Entry) -> bool {
    entry.kind == Kind::Folder
        && old
            .get(name)
            .is_none_or(|was| was.kind != Kind::Folder || was.id != entry.id)
}

/// What the engine would hold of the folder: each folder's entries, as
/// last read, and each watch's folder.
struct Model {
    root: Root,
    opts: Options,
    watcher: Arc<Watcher>,
    folders: HashMap<PathBuf, Entries>,
    by_wd: HashMap<i32, PathBuf>,
    /// The most folders watched at once, and watches refused.
    most: usize,
    refused: u64,
}

impl Model {
    fn note_watch(&mut self, watch: Watch, rel: &Path) {
        match watch {
            Watch::Watched(wd) => {
                self.by_wd.insert(wd, rel.to_path_buf());
            }
            Watch::Unasked => {}
            _ => self.refused += 1,
        }
        self.most = self.most.max(self.watcher.watching());
    }

    /// Crawl the folder at `rel` and below, watched; each folder found
    /// logged with its entries all added. Gives the folders crawled.
    fn crawl(&mut self, rel: &Path, cost: &mut Cost) -> Vec<PathBuf> {
        let Ok(crawl) = crawl_below(&self.root, rel, &self.opts, &Cancel::default()) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for folder in crawl.folders {
            self.note_watch(folder.watch, &folder.rel);
            let Outcome::Listed(listing) = folder.outcome else {
                continue;
            };
            let entries: Entries = listing.iter().map(|(n, e)| (n.to_vec(), *e)).collect();
            cost.reads += 1;
            cost.log(
                &folder.rel,
                folder.stat.as_ref().map(facts),
                diff(&Entries::new(), &entries),
            );
            found.push(folder.rel.clone());
            self.folders.insert(folder.rel, entries);
        }
        found
    }

    /// Let go of the folder at `rel` and everything below it.
    fn drop_subtree(&mut self, rel: &Path) -> Vec<PathBuf> {
        let gone: Vec<PathBuf> = self
            .folders
            .keys()
            .filter(|p| p.starts_with(rel))
            .cloned()
            .collect();
        for p in &gone {
            self.folders.remove(p);
        }
        let watcher = &self.watcher;
        self.by_wd.retain(|&wd, p| {
            let keep = !p.starts_with(rel);
            if !keep {
                watcher.unwatch(wd);
            }
            keep
        });
        gone
    }

    /// Read the folder at `rel` again, as the engine applies what it heard;
    /// gives every folder touched: it, those made below it, those gone.
    fn reread(&mut self, rel: &Path, cost: &mut Cost) -> Vec<PathBuf> {
        if !self.folders.contains_key(rel) {
            // Gone with a folder above it, read before it.
            return Vec::new();
        }
        let (stat, outcome, watch) = read_folder_watched(&self.root, rel, &self.opts);
        self.note_watch(watch, rel);
        let old = self.folders.remove(rel).unwrap_or_default();
        let Outcome::Listed(listing) = outcome else {
            let mut touched = self.drop_subtree(rel);
            touched.push(rel.to_path_buf());
            return touched;
        };
        let new: Entries = listing.iter().map(|(n, e)| (n.to_vec(), *e)).collect();
        let mut touched = vec![rel.to_path_buf()];
        for (name, entry) in &old {
            if entry.kind == Kind::Folder
                && new
                    .get(name)
                    .is_none_or(|now| now.kind != Kind::Folder || now.id != entry.id)
            {
                touched.extend(self.drop_subtree(&rel.join(component(name))));
            }
        }
        let fresh: Vec<PathBuf> = new
            .iter()
            .filter(|(name, entry)| is_new_folder(&old, name, entry))
            .map(|(name, _)| rel.join(component(name)))
            .collect();
        cost.reads += 1;
        let changes = diff(&old, &new);
        if !changes.is_empty() {
            cost.log(rel, stat.as_ref().map(facts), changes);
        }
        self.folders.insert(rel.to_path_buf(), new);
        for folder in fresh {
            touched.extend(self.crawl(&folder, cost));
        }
        touched
    }
}

/// Changes logged in batches: each folder's once its batch closes.
struct Batched {
    quiet: Duration,
    most: Duration,
    /// Each folder's entries as of the last batch logged.
    logged: HashMap<PathBuf, Entries>,
    pending: BTreeSet<PathBuf>,
    first: Option<Instant>,
    last: Option<Instant>,
    batches: u64,
    cost: Cost,
}

impl Batched {
    fn new(quiet: Duration, most: Duration, model: &Model) -> Batched {
        Batched {
            quiet,
            most,
            logged: model.folders.clone(),
            pending: BTreeSet::new(),
            first: None,
            last: None,
            batches: 0,
            cost: Cost::default(),
        }
    }

    fn note(&mut self, touched: &[PathBuf], now: Instant) {
        if touched.is_empty() {
            return;
        }
        self.pending.extend(touched.iter().cloned());
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// Close the batch if its time has come (or `now` is none: at the end).
    fn close(&mut self, model: &Model, now: Option<Instant>) {
        let (Some(first), Some(last)) = (self.first, self.last) else {
            return;
        };
        if let Some(now) = now
            && now.duration_since(last) < self.quiet
            && now.duration_since(first) < self.most
        {
            return;
        }
        self.batches += 1;
        // Its facts' size is fixed: any stand in.
        let some_facts = Some(Facts {
            id: FileId::default(),
            ctime: Time::default(),
            mtime: Time::default(),
            owner: 0,
            group: 0,
            mode: 0,
        });
        for rel in std::mem::take(&mut self.pending) {
            let old = self.logged.remove(&rel).unwrap_or_default();
            // Gone, it has no record of its own: its parent's says so.
            if let Some(new) = model.folders.get(&rel) {
                self.cost.reads += 1;
                let changes = diff(&old, new);
                if !changes.is_empty() {
                    self.cost.log(&rel, some_facts, changes);
                }
                self.logged.insert(rel, new.clone());
            }
        }
        self.first = None;
        self.last = None;
    }
}

/// Watch `folder`, run `command` in it, and say what its changes cost.
pub fn run(folder: &Path, command: &[String]) -> Result<(), String> {
    let (program, args) = command
        .split_first()
        .ok_or("churn takes a command to run, after --")?;
    let root = Root::open(folder, Exclusions::none()).map_err(|e| e.to_string())?;
    let watcher =
        Arc::new(Watcher::new(1, Watcher::share()).map_err(|e| format!("cannot watch: {e}"))?);
    watcher.set_scopes(vec![root.path().to_path_buf()]);
    let opts = Options {
        watcher: Some(watcher.clone()),
        ..Options::default()
    };
    let mut model = Model {
        root,
        opts,
        watcher,
        folders: HashMap::new(),
        by_wd: HashMap::new(),
        most: 0,
        refused: 0,
    };
    model.crawl(Path::new(""), &mut Cost::default());
    let at_start = model.folders.len();
    let windows = [
        (Duration::from_millis(100), Duration::from_secs(1)),
        (Duration::from_secs(1), Duration::from_secs(10)),
        (Duration::from_secs(5), Duration::from_secs(60)),
    ];
    let mut batched: Vec<Batched> = windows
        .iter()
        .map(|&(quiet, most)| Batched::new(quiet, most, &model))
        .collect();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run the command: {e}"))?;
    let listened = listen(&mut model, &mut batched, &mut child)?;
    for b in &mut batched {
        b.close(&model, None);
    }
    print(&Printed {
        busy: listened.busy,
        ok: listened.ok,
        at_start,
        at_end: model.folders.len(),
        most: model.most,
        refused: model.refused,
        events: listened.events,
        overflows: listened.overflows,
        at_once: &listened.at_once,
        batched: &batched,
    });
    Ok(())
}

/// What listening while the command ran heard and applied.
struct Listened {
    busy: Duration,
    ok: bool,
    events: u64,
    overflows: u64,
    at_once: Cost,
}

/// Apply what is heard until the command has ended and every batch could
/// close.
fn listen(
    model: &mut Model,
    batched: &mut [Batched],
    child: &mut Child,
) -> Result<Listened, String> {
    let start = Instant::now();
    let mut listened = Listened {
        busy: Duration::ZERO,
        ok: false,
        events: 0,
        overflows: 0,
        at_once: Cost::default(),
    };
    let mut ended = false;
    let mut heard = Vec::new();
    let mut last_heard = start;
    let tick = Timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    loop {
        let fd = model.watcher.fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let _ = poll(&mut fds, Some(&tick));
        heard.clear();
        model.watcher.read(&mut heard).map_err(|e| e.to_string())?;
        let now = Instant::now();
        listened.events += heard.len() as u64;
        let mut named = BTreeSet::new();
        for h in &heard {
            match h {
                Heard::Changed(wd) | Heard::Entry(wd, _) => {
                    if let Some(rel) = model.by_wd.get(wd) {
                        named.insert(rel.clone());
                    }
                }
                Heard::Gone(wd) => {
                    model.by_wd.remove(wd);
                }
                Heard::Moved(_) | Heard::Cookie(_) => {}
                Heard::Overflow => listened.overflows += 1,
            }
        }
        if !heard.is_empty() {
            last_heard = now;
        }
        // In path order, a folder before those below it: one gone with its
        // parent is not read.
        let mut touched = Vec::new();
        for rel in named {
            touched.extend(model.reread(&rel, &mut listened.at_once));
        }
        for b in batched.iter_mut() {
            b.note(&touched, now);
            b.close(model, Some(now));
        }
        if !ended && let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            ended = true;
            listened.busy = now.duration_since(start);
            listened.ok = status.success();
        }
        if ended && now.duration_since(last_heard) > Duration::from_secs(6) {
            return Ok(listened);
        }
    }
}
struct Printed<'a> {
    busy: Duration,
    ok: bool,
    at_start: usize,
    at_end: usize,
    most: usize,
    refused: u64,
    events: u64,
    overflows: u64,
    at_once: &'a Cost,
    batched: &'a [Batched],
}

/// How long the journal's 16 MB of log would last at what was logged.
fn lasts(cost: &Cost, busy: Duration) -> String {
    if cost.bytes == 0 {
        return "for ever".into();
    }
    let rate = cost.bytes as f64 / busy.as_secs_f64().max(0.001);
    time(Duration::from_secs_f64(16.0 * 1024.0 * 1024.0 / rate))
}

fn print(p: &Printed<'_>) {
    println!(
        "command: {}, busy {}",
        if p.ok { "succeeded" } else { "failed" },
        time(p.busy)
    );
    println!(
        "folders: {} watched at the start, {} at the end, {} watched at most, {} watches refused",
        count(p.at_start as u64),
        count(p.at_end as u64),
        count(p.most as u64),
        count(p.refused)
    );
    println!(
        "heard: {} events, {} overflows{}",
        count(p.events),
        count(p.overflows),
        if p.overflows > 0 {
            " (what follows is less than happened)"
        } else {
            ""
        }
    );
    println!("applied                      reads   records   changes        log   16 MB last");
    let row = |label: &str, c: &Cost| {
        println!(
            "{label:<24} {:>9} {:>9} {:>9} {:>10} {:>12}",
            count(c.reads),
            count(c.records),
            count(c.changes),
            bytes(c.bytes),
            lasts(c, p.busy)
        );
    };
    row("at once", p.at_once);
    for b in p.batched {
        row(
            &format!("batches {}/{}", time(b.quiet), time(b.most)),
            &b.cost,
        );
    }
    for b in p.batched {
        println!(
            "batches {}/{}: {} batches",
            time(b.quiet),
            time(b.most),
            count(b.batches)
        );
    }
}
