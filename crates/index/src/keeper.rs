//! The index of one root kept on disk with its log (`docs/DESIGN.md`,
//! decision 8): opened from what was saved, its log replayed, or built
//! anew; refreshed, each change logged; saved, the chunks a change touched
//! written; asked what changed since a token; purged of a folder made
//! private.
//!
//! A save that fails leaves the index in memory as it is, and what is on
//! disk as it was: the last manifest, the files it names and the log, so
//! that a start after it replays to where the log ends. A log that cannot
//! be written ends its epoch: the next save starts another, and every
//! token of the old one is too old, never answered with a gap.

use std::path::{Path, PathBuf};

use skwird_crawl::{Cancel, Entry, Error, Options, Root, Time, Why, crawl_all};

use crate::index::Index;
use crate::log::{self, Log, Op, Record, Token};
use crate::store::{self, Mark};
use crate::tree::Limits;
use crate::update::{self, Proof, Refreshed, Seen};

/// A log past this many bytes at a save starts another epoch: the
/// journal keeps about as much.
const LOG_LIMIT: u64 = 16 << 20;

/// What changed below a folder, for the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An entry added, removed or changed: for a removal, what it was. A
    /// folder removed takes what was below it.
    Entry { path: PathBuf, op: Op, entry: Entry },
    /// A folder whose entries are not covered from now on, and why.
    NotCovered { path: PathBuf, why: Why },
}

/// A token from before what the log holds, or from another epoch: what
/// changed since is not known, and the client walks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooOld;

/// How an index was opened.
#[derive(Debug)]
pub enum Opened {
    /// Read back, and its log replayed past what its chunks hold.
    Loaded { replayed: usize },
    /// Built anew from a crawl: there was none, or what there was could
    /// not be read (why).
    Built { because: Option<String> },
}

/// The index of one root, its log, and where they are kept.
pub struct Keeper {
    dir: PathBuf,
    root: Root,
    index: Index,
    /// None once an append failed: changes are kept in memory, and the
    /// next save starts another epoch.
    log: Option<Log>,
    epoch: u64,
    opts: Options,
    limits: Limits,
}

/// An epoch not `other`: the time, and the process.
fn fresh_epoch(other: u64) -> u64 {
    let now = Time::now();
    let mut epoch = (now.sec.unsigned_abs() << 30)
        ^ u64::from(now.nsec)
        ^ (u64::from(std::process::id()) << 48);
    if epoch == other {
        epoch = epoch.wrapping_add(1);
    }
    epoch
}

fn proven_at(proof: Proof) -> Time {
    match proof {
        Proof::Checked(t) | Proof::AsOf(t) => t,
    }
}

impl Keeper {
    /// The index of `root` kept in `dir`: read back and its log replayed,
    /// as of when its names were last proven; or, where none is saved or
    /// what is saved cannot be read, built anew from a crawl, and saved.
    pub fn open(
        dir: &Path,
        root: Root,
        opts: Options,
        limits: Limits,
        cancel: &Cancel,
    ) -> Result<(Keeper, Opened), Error> {
        let because = match store::load(dir, limits) {
            Ok((index, mark)) => match Keeper::replayed(dir, index, &mark) {
                Some((index, log, replayed)) => {
                    let mut keeper = Keeper {
                        dir: dir.to_path_buf(),
                        root,
                        index,
                        epoch: log.as_ref().map_or(mark.epoch, Log::epoch),
                        log,
                        opts,
                        limits,
                    };
                    if keeper.log.is_none() {
                        // The journal cannot go on from a log not found.
                        let _ = keeper.new_epoch();
                    }
                    return Ok((keeper, Opened::Loaded { replayed }));
                }
                None => Some("its log does not belong with its chunks".to_owned()),
            },
            Err(store::LoadError::Missing) => None,
            Err(e) => Some(e.to_string()),
        };
        let start = Time::now();
        let crawl = crawl_all(&root, &opts, cancel)?;
        let mut index = crate::build(crawl, limits);
        let tells = root.fs_name() != "fuse";
        index.set_proof(if tells {
            Proof::Checked(start)
        } else {
            Proof::AsOf(start)
        });
        index.set_crawled_at(start);
        let mut keeper = Keeper {
            dir: dir.to_path_buf(),
            root,
            index,
            log: None,
            epoch: 0,
            opts,
            limits,
        };
        // Saved at once, so that the next start loads it; a save that
        // fails is tried again at the next.
        let _ = keeper.save();
        Ok((keeper, Opened::Built { because }))
    }

    /// `index` with the log of `mark`'s epoch replayed past what its
    /// chunks hold, the log open to append, and how many records were
    /// taken; none if a record does not apply (the log and the chunks are
    /// not of one index). A log missing, or starting past what the chunks
    /// hold, leaves the index as saved, without a log.
    fn replayed(dir: &Path, mut index: Index, mark: &Mark) -> Option<(Index, Option<Log>, usize)> {
        let path = dir.join(log::file_name(mark.epoch));
        let read = Log::read(&path)
            .ok()
            .flatten()
            .filter(|r| r.epoch == mark.epoch && r.first <= mark.log_seq + 1);
        let mut proven = mark.proven_at;
        let mut replayed = 0;
        let mut log = None;
        if let Some(read) = read {
            for logged in read.records.iter().filter(|l| l.seq > mark.log_seq) {
                match &logged.record {
                    Record::Folder(f) => index.replay(f).ok()?,
                    Record::Proven(at) => proven = *at,
                }
                replayed += 1;
            }
            log = Log::reopen(&path, &read).ok();
        }
        // As of when its names were last proven, what replayed included.
        index.set_proof(Proof::AsOf(proven));
        index.set_crawled_at(mark.crawled_at);
        Some((index, log, replayed))
    }

    /// Start another epoch of the log: every token of the one before is
    /// too old from now on.
    fn new_epoch(&mut self) -> std::io::Result<()> {
        self.log = None;
        store::make_dir(&self.dir)?;
        let epoch = fresh_epoch(self.epoch);
        let log = Log::create(&self.dir, epoch, 1)?;
        self.epoch = epoch;
        self.log = Some(log);
        Ok(())
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn root(&self) -> &Root {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Root {
        &mut self.root
    }

    /// Read the folder at `rel` again at the next refresh, its exclusion
    /// taken back: until then it is not covered, not read yet.
    pub fn readmit(&mut self, rel: &Path) {
        if let Some(folder) = self.index.folder_at(rel) {
            self.index
                .reread(folder, None, Seen::NotCovered(Why::Unread));
        }
    }

    /// A snapshot of the index, for queries to read while it changes.
    pub fn snapshot(&self) -> Index {
        self.index.clone()
    }

    /// The token of what the index holds now, or none while its log is
    /// broken.
    pub fn token(&self) -> Option<Token> {
        self.log.as_ref().map(Log::token)
    }

    /// Prove the index current ([`update::refresh`]), logging each change
    /// and, once done, when its names were proven.
    pub fn refresh(&mut self, cancel: &Cancel) -> Result<Refreshed, Error> {
        let done = update::refresh(&mut self.index, &mut self.root, &self.opts, cancel)?;
        // The time the index now says it was proven at, exactly.
        let proven = proven_at(self.index.proof());
        self.append(done.records.iter().cloned().chain([Record::Proven(proven)]));
        Ok(done)
    }

    /// Append `records` to the log, made durable together; a log that
    /// fails ends its epoch.
    fn append(&mut self, records: impl IntoIterator<Item = Record>) {
        let Some(log) = self.log.as_mut() else {
            return;
        };
        let now = Time::now();
        let mut ok = true;
        for record in records {
            if log.append(now, record).is_err() {
                ok = false;
                break;
            }
        }
        if !ok || log.sync().is_err() {
            self.log = None;
        }
    }

    /// Save the index: the chunks a change touched and the names given
    /// since written, the manifest switched to say how far into the log
    /// they go, the files no manifest names removed, and logs of other
    /// epochs let go of. A log grown past its limit starts another epoch.
    pub fn save(&mut self) -> std::io::Result<()> {
        if self.log.is_none() {
            self.new_epoch()?;
        }
        let log = self.log.as_ref().expect("a log was just made");
        let mark = Mark {
            saved_at: Time::now(),
            crawled_at: self.index.crawled_at(),
            proven_at: proven_at(self.index.proof()),
            epoch: log.epoch(),
            log_seq: log.next() - 1,
        };
        store::save(&mut self.index, &self.dir, &mark)?;
        let size = std::fs::metadata(log.path()).map_or(0, |m| m.len());
        if size > LOG_LIMIT {
            self.new_epoch()?;
            let log = self.log.as_ref().expect("a log was just made");
            store::save(
                &mut self.index,
                &self.dir,
                &Mark {
                    epoch: log.epoch(),
                    log_seq: log.next() - 1,
                    ..mark
                },
            )?;
        }
        self.remove_other_logs();
        Ok(())
    }

    fn remove_other_logs(&self) {
        let ours = log::file_name(self.epoch);
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("log-") && name != ours {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// What changed below `below` since `since`, and the token to ask from
    /// next; too old if the log does not reach back to it.
    pub fn changes(&self, below: &Path, since: Token) -> Result<(Vec<Event>, Token), TooOld> {
        let log = self.log.as_ref().ok_or(TooOld)?;
        let now = log.token();
        if since.epoch != log.epoch() || since.seq + 1 < log.first() || since.seq > now.seq {
            return Err(TooOld);
        }
        let read = Log::read(log.path()).ok().flatten().ok_or(TooOld)?;
        let mut events = Vec::new();
        for logged in read.records.iter().filter(|l| l.seq > since.seq) {
            let Record::Folder(f) = &logged.record else {
                continue;
            };
            if !f.path.starts_with(below) {
                continue;
            }
            if let crate::block::State::NotCovered(why) = f.state {
                events.push(Event::NotCovered {
                    path: f.path.clone(),
                    why,
                });
            }
            for change in &f.changes {
                events.push(Event::Entry {
                    path: f.path.join(crate::index::component(&change.name)),
                    op: change.op,
                    entry: change.entry,
                });
            }
        }
        Ok((events, now))
    }

    /// Let go of the folder at `rel` and everything below it, at once and
    /// on disk: made private, or registered by a client (`why`). Its names
    /// are dropped from the names' files, the log that held them ends its
    /// epoch, and the index is saved without them.
    pub fn purge(&mut self, rel: &Path, why: Why) -> std::io::Result<()> {
        let Some(folder) = self.index.folder_at(rel) else {
            return Ok(());
        };
        self.index.reread(folder, None, Seen::NotCovered(why));
        self.index.blank_dead_names();
        self.new_epoch()?;
        self.save()
    }

    /// How large its chunks grow.
    pub fn limits(&self) -> Limits {
        self.limits
    }
}
