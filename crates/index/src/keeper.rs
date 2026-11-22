//! The index of one root kept on disk with its log (`docs/DESIGN.md`,
//! decision 8): opened from what was saved, its log replayed, or built
//! anew; refreshed, each change logged; saved, the chunks a change touched
//! written; asked what changed since a token; purged of a folder made
//! private.
//!
//! The folders it holds as excluded are always those its root's
//! exclusions name: made to hold to them when it is opened and whenever
//! they change ([`Keeper::set_exclusions`]), never only by the events
//! that changed them. An exclusion added or taken back while no engine
//! ran, or one whose purge a kill cut short, holds from the next open.
//!
//! A save that fails leaves the index in memory as it is, and what is on
//! disk as it was: the last manifest, the files it names and the log, so
//! that a start after it replays to where the log ends. A log that cannot
//! be written ends its epoch: the next save starts another, and every
//! token of the old one is too old, never answered with a gap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use skwird_crawl::{Cancel, Entry, Error, Exclusions, Options, Root, Time, Watch, Why};

use crate::block::{FolderId, State};
use crate::index::Index;
use crate::log::{self, Log, Op, Record, Token};
use crate::store::{self, Mark};
use crate::tree::Limits;
use crate::update::{self, HeardFolder, Proof, Refreshed, Seen};

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
    /// Read back, its log replayed past what its chunks hold, and made to
    /// hold to its root's exclusions.
    Loaded {
        replayed: usize,
        reconciled: Reconciled,
    },
    /// Built anew from a crawl: there was none, or what there was could
    /// not be read (why).
    Built { because: Option<String> },
}

/// What making an index hold to its exclusions did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Folders the exclusions name that the index held: purged.
    pub purged: usize,
    /// Folders the index held as excluded that the exclusions no longer
    /// name: not covered, not read yet, until the next refresh reads them.
    pub readmitted: usize,
    /// The purge could not be saved: the index in memory is without the
    /// folders' names, and the next save writes it so.
    pub unsaved: bool,
}

/// A subtree watched (`docs/PLAN.md`, phase 2, step 3): live once every
/// folder in it is watched and it has been proven current after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    /// Its folder's path below the root.
    pub scope: PathBuf,
    /// Since when every change below it is heard: none while it is not,
    /// not installed yet, or since an overflow until a check.
    pub since: Option<Time>,
    /// Why it cannot be live: a folder in it past the budget of watches,
    /// past the user's limit, or whose watch failed. It is checked, then.
    pub short: Option<Watch>,
    /// Whether every folder in it has been watched and compared.
    installed: bool,
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
    /// Each watch descriptor's folder.
    watches: HashMap<i32, FolderId>,
    /// Each folder's descriptor: a folder renumbered, or a number given
    /// again, never leaves a descriptor behind.
    by_folder: HashMap<FolderId, i32>,
    live: Vec<Live>,
    /// The access ACLs read, by folder identity, each with the change
    /// time its folder had: read again once it moves.
    acls: HashMap<skwird_crawl::FileId, (Time, skwird_crawl::acl::Acl)>,
    /// The log's epoch and last record when the index was last saved, or
    /// found saved: none until it is ([`Keeper::save`]).
    saved: Option<(u64, u64)>,
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
                        watches: HashMap::new(),
                        by_folder: HashMap::new(),
                        live: Vec::new(),
                        acls: HashMap::new(),
                        saved: Some((mark.epoch, mark.log_seq)),
                    };
                    if keeper.log.is_none() {
                        // The journal cannot go on from a log not found.
                        let _ = keeper.new_epoch();
                    }
                    // The exclusions may have changed while no engine ran.
                    let reconciled = keeper.reconcile();
                    keeper.read_acls(None);
                    keeper.root.let_go();
                    return Ok((
                        keeper,
                        Opened::Loaded {
                            replayed,
                            reconciled,
                        },
                    ));
                }
                None => Some("its log does not belong with its chunks".to_owned()),
            },
            Err(store::LoadError::Missing) => None,
            Err(e) => Some(e.to_string()),
        };
        let start = Time::now();
        // Packed as it is read: the crawl is never held whole.
        let built = crate::build::crawl_and_build(&root, &opts, cancel, limits)?;
        let mut index = built.index;
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
            watches: HashMap::new(),
            by_folder: HashMap::new(),
            live: Vec::new(),
            acls: HashMap::new(),
            saved: None,
        };
        keeper.note(&built.watches);
        // Saved at once, so that the next start loads it; a save that
        // fails is tried again at the next.
        let _ = keeper.save();
        keeper.read_acls(None);
        keeper.root.let_go();
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

    /// Take `exclusions` from now on, and make the index hold to them: a
    /// folder they name is purged at once, and one they no longer name is
    /// read at the next refresh.
    pub fn set_exclusions(&mut self, exclusions: Exclusions) -> Reconciled {
        self.root.set_exclusions(exclusions);
        self.reconcile()
    }

    /// Make the folders the index holds as excluded those the root's
    /// exclusions name, whatever happened between: an exclusion added or
    /// taken back while no engine ran, a purge or a readmission a kill cut
    /// short. Only a folder's parent read again would find them otherwise.
    fn reconcile(&mut self) -> Reconciled {
        let base = self.root.path();
        let exclusions = self.root.exclusions();
        let excluded = |state: Option<State>| state == Some(State::NotCovered(Why::Excluded));
        // Named, and held: purged. The root itself is never its own hole.
        let purge: Vec<FolderId> = exclusions
            .paths()
            .filter_map(|p| p.strip_prefix(base).ok())
            .filter(|rel| !rel.as_os_str().is_empty())
            .filter_map(|rel| self.index.folder_at(rel))
            .filter(|&f| !excluded(self.index.state_of(f)))
            .collect();
        // Excluded, and named no more: read again. An excluded folder holds
        // nothing below it, so none of these is below another; one below a
        // folder purged goes with it.
        let mut readmit: Vec<FolderId> = Vec::new();
        for (_, chunk) in self.index.tree().chunks() {
            let view = chunk.view();
            readmit.extend(
                (0..chunk.len())
                    .filter(|&b| excluded(Some(view.state(b))))
                    .map(|b| view.folder(b)),
            );
        }
        readmit.retain(|&f| {
            let id = self.index.facts_of(f).map(|facts| facts.id);
            !exclusions.holds(id, &base.join(self.index.path_of(f)))
        });
        for &folder in &purge {
            self.index
                .reread(folder, None, Seen::NotCovered(Why::Excluded));
        }
        readmit.retain(|&f| self.index.tree().loc(f).is_some());
        for &folder in &readmit {
            self.index
                .reread(folder, None, Seen::NotCovered(Why::Unread));
        }
        let unsaved = !purge.is_empty() && self.purged().is_err();
        Reconciled {
            purged: purge.len(),
            readmitted: readmit.len(),
            unsaved,
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
        let done = self.refresh_held(cancel);
        self.read_acls(None);
        self.root.let_go();
        done
    }

    fn refresh_held(&mut self, cancel: &Cancel) -> Result<Refreshed, Error> {
        let done = update::refresh(&mut self.index, &mut self.root, &self.opts, cancel)?;
        // The time the index now says it was proven at, exactly.
        let proven = proven_at(self.index.proof());
        self.append(done.records.iter().cloned().chain([Record::Proven(proven)]));
        self.note(&done.watches);
        self.prune();
        // Proven after an overflow, an installed subtree is live again:
        // its watches stayed, and what came of the events lost is read.
        if let Proof::Checked(at) = self.index.proof() {
            for live in &mut self.live {
                if live.installed && live.short.is_none() && live.since.is_none() {
                    live.since = Some(at);
                }
            }
        }
        // A subtree whose folder was not there is tried again: made since,
        // the check found it.
        let pending: Vec<PathBuf> = self
            .live
            .iter()
            .filter(|l| !l.installed && self.index.folder_at(&l.scope).is_some())
            .map(|l| l.scope.clone())
            .collect();
        for scope in pending {
            self.install(&scope, cancel)?;
        }
        self.prune();
        self.publish_live();
        Ok(done)
    }

    /// Watch, from now on, the subtrees at `scopes` (below the root),
    /// through the watcher of [`Options::watcher`], which must already
    /// want them. Each new one is installed: every folder in it watched
    /// and compared with the index ([`update::install`]), live from when
    /// that began if every folder was watched. A subtree no longer named
    /// lets go of the watches only it held.
    pub fn set_scopes(&mut self, scopes: &[PathBuf], cancel: &Cancel) -> Result<Refreshed, Error> {
        let done = self.set_scopes_held(scopes, cancel);
        self.read_acls(None);
        self.root.let_go();
        done
    }

    fn set_scopes_held(&mut self, scopes: &[PathBuf], cancel: &Cancel) -> Result<Refreshed, Error> {
        self.live.retain(|l| scopes.contains(&l.scope));
        let held: Vec<PathBuf> = self.live.iter().map(|l| l.scope.clone()).collect();
        self.unwatch_where(|path| !held.iter().any(|s| path.starts_with(s)));
        let mut done = Refreshed::default();
        for scope in scopes.iter().filter(|s| !held.contains(s)) {
            self.live.push(Live {
                scope: scope.clone(),
                since: None,
                short: None,
                installed: false,
            });
            let installed = self.install(scope, cancel)?;
            done.records.extend(installed.records);
            done.watches.extend(installed.watches);
            done.checked += installed.checked;
            done.reread += installed.reread;
            done.crawled += installed.crawled;
        }
        self.prune();
        self.publish_live();
        Ok(done)
    }

    /// Install the subtree at `scope` ([`update::install`]): live from
    /// when this began if every folder in it was watched. A folder not
    /// there is not live, nor installed: each refresh tries it again.
    fn install(&mut self, scope: &Path, cancel: &Cancel) -> Result<Refreshed, Error> {
        let start = Time::now();
        let installed =
            update::install(&mut self.index, &mut self.root, &self.opts, cancel, scope)?;
        self.append(installed.records.iter().cloned());
        self.note(&installed.watches);
        let there = self.index.folder_at(scope).is_some();
        if let Some(live) = self.live.iter_mut().find(|l| l.scope == scope) {
            live.installed = there;
            if there && live.short.is_none() {
                live.since = Some(start);
            }
        }
        Ok(installed)
    }

    /// The whole root heard from `since` on (fanotify, phase 3): every
    /// filesystem below it marked before `since`, and the index proven
    /// current after. Live from then until an overflow or a failure to
    /// open what was heard ([`Keeper::overflowed`]), then from the check
    /// after.
    pub fn heard_whole(&mut self, since: Time) {
        self.live = vec![Live {
            scope: PathBuf::new(),
            since: Some(since),
            short: None,
            installed: true,
        }];
        self.publish_live();
    }

    /// The whole root no longer heard: its filesystem's mark is gone with
    /// its superblock, and no check brings it back.
    pub fn unheard(&mut self) {
        self.live.clear();
        self.publish_live();
    }

    /// The folder at `rel` below the root, if the index holds one there of
    /// identity `id`: a folder heard of, found by its path and known by its
    /// identity, so that a folder put in its place is not taken for it.
    pub fn folder_of(&self, rel: &Path, id: skwird_crawl::FileId) -> Option<FolderId> {
        let folder = self.index.folder_at(rel)?;
        (self.index.facts_of(folder)?.id == id).then_some(folder)
    }

    /// Its index's shape ([`crate::tree::Tree::shape`]), for
    /// [`Keeper::holds`].
    pub fn shape(&self) -> u64 {
        self.index.tree().shape()
    }

    /// Whether `folder`, found ([`Keeper::folder_of`]) with the identity
    /// `id` while the index had the shape `shape`, is still what finding
    /// it again would give: no folder made, moved or let go of since, so
    /// that none of the index holds that identity elsewhere (a folder
    /// moved, read at its new place before its old parent is read again),
    /// and it is still that identity.
    pub fn holds(&self, folder: FolderId, id: skwird_crawl::FileId, shape: u64) -> bool {
        self.shape() == shape && self.index.facts_of(folder).is_some_and(|f| f.id == id)
    }

    /// The folders whose ACL a read again may have changed, by what its
    /// records say: each folder read or crawled, and each folder an entry
    /// of them made or changed. Not every folder those hold: their ACLs
    /// change only with their entries, which the records name (DESIGN 8).
    fn acl_targets(&self, done: &Refreshed) -> Vec<FolderId> {
        let mut looked = Vec::new();
        for record in &done.records {
            let Record::Folder(folder) = record else {
                continue;
            };
            looked.extend(self.index.folder_at(&folder.path));
            for change in &folder.changes {
                if change.op != Op::Removed && change.entry.kind == skwird_crawl::Kind::Folder {
                    let name = crate::index::component(&change.name);
                    looked.extend(self.index.folder_at(&folder.path.join(name)));
                }
            }
        }
        looked.sort_unstable();
        looked.dedup();
        looked
    }

    /// The access ACLs of the folders whose mode says they have one, read
    /// for the system service (`Options::acls`, DESIGN 4): of every folder,
    /// or of those `only` names ([`Keeper::acl_targets`]). Each is read
    /// again only once its folder's change time moved, which a change of
    /// its ACL moves. One that cannot be read is left out: its folder is
    /// not known then, never answered as its mode alone says.
    fn read_acls(&mut self, only: Option<&[FolderId]>) {
        if !self.opts.acls {
            return;
        }
        let folders: Vec<FolderId> = match only {
            None => self
                .index
                .root()
                .map(|top| self.index.acl_folders_below(top))
                .unwrap_or_default(),
            Some(looked) => looked.to_vec(),
        };
        // Kept for a pass over every folder alone, to let go of the rest.
        let mut seen = std::collections::HashSet::new();
        let mut changed = false;
        for folder in folders {
            let Some(facts) = self.index.facts_of(folder) else {
                continue;
            };
            if only.is_none() {
                seen.insert(facts.id);
            }
            if facts.mode & skwird_crawl::ACL == 0 {
                changed |= self.acls.remove(&facts.id).is_some();
                continue;
            }
            if self
                .acls
                .get(&facts.id)
                .is_some_and(|(at, _)| *at == facts.ctime)
            {
                continue;
            }
            changed = true;
            match self.root.access_acl(&self.index.path_of(folder), facts.id) {
                Some(acl) => {
                    self.acls.insert(facts.id, (facts.ctime, acl));
                }
                None => {
                    self.acls.remove(&facts.id);
                }
            }
        }
        // Every folder looked at: those no longer there have no ACL here.
        if only.is_none() {
            let before = self.acls.len();
            self.acls.retain(|id, _| seen.contains(id));
            changed |= self.acls.len() != before;
        }
        // Made again for the snapshots only when it changed: it holds every
        // ACL of the root.
        if !changed && only.is_some() {
            return;
        }
        let acls = self
            .acls
            .iter()
            .map(|(id, (_, acl))| (*id, acl.clone()))
            .collect();
        self.index.set_acls(std::sync::Arc::new(acls));
    }

    /// The subtrees watched, live or not.
    pub fn live(&self) -> &[Live] {
        &self.live
    }

    /// The folder a watch descriptor watches, if this index holds it.
    pub fn watched(&self, wd: i32) -> Option<FolderId> {
        self.watches.get(&wd).copied()
    }

    /// Apply what was heard in `folders`: each read again, watched, its
    /// new folders crawled, each change logged.
    pub fn heard(&mut self, folders: &[FolderId], cancel: &Cancel) -> Result<Refreshed, Error> {
        let whole: Vec<HeardFolder> = folders.iter().map(|&f| (f, None)).collect();
        self.heard_named(&whole, cancel)
    }

    /// [`Keeper::heard`], each folder given the names its events gave: of
    /// what it held, those alone are stat-ed again ([`update::reread_named`]).
    pub fn heard_named(
        &mut self,
        folders: &[HeardFolder],
        cancel: &Cancel,
    ) -> Result<Refreshed, Error> {
        let done = update::reread_named(&mut self.index, &self.root, &self.opts, cancel, folders);
        if let Ok(d) = &done {
            let looked = self.acl_targets(d);
            self.read_acls(Some(&looked));
        }
        self.root.let_go();
        let done = done?;
        self.append(done.records.iter().cloned());
        self.note(&done.watches);
        self.prune();
        self.publish_live();
        Ok(done)
    }

    /// The mount table changed (`POLLPRI` on `/proc/self/mountinfo`): read
    /// it again, and read again the folder at each mount point made or
    /// taken below the root. A mount made is a hole at once, its names the
    /// mount's and not the folder's; a mount taken lets the folder be read
    /// and crawled.
    pub fn mounts_changed(&mut self, cancel: &Cancel) -> Result<Refreshed, Error> {
        let before = self.root.mounts_below();
        let reread = self.root.reread_mounts();
        self.root.let_go();
        reread.map_err(Error::Root)?;
        let after = self.root.mounts_below();
        let mut folders: Vec<FolderId> = before
            .iter()
            .filter(|p| !after.contains(p))
            .chain(after.iter().filter(|p| !before.contains(p)))
            .filter_map(|rel| self.index.folder_at(rel))
            .collect();
        folders.sort_unstable();
        folders.dedup();
        self.heard(&folders, cancel)
    }

    /// Events were dropped: no subtree is live until the next refresh.
    pub fn overflowed(&mut self) {
        for live in &mut self.live {
            live.since = None;
        }
        self.publish_live();
    }

    /// `folder` moved, or its watch went: if it is a watched subtree's own
    /// folder, which no watch above it follows, that subtree is not live
    /// until the next refresh, and this says so.
    pub fn lost(&mut self, folder: FolderId) -> bool {
        let path = self.index.path_of(folder);
        let mut lost = false;
        for live in &mut self.live {
            if live.scope == path && live.since.is_some() {
                live.since = None;
                lost = true;
            }
        }
        self.publish_live();
        lost
    }

    /// What came of watching folders: each descriptor's folder kept, and
    /// each subtree holding a folder not watched made short of live.
    fn note(&mut self, watches: &[(FolderId, Watch)]) {
        // With no watcher, what changes is heard of whole filesystems
        // (fanotify, phase 3): a folder's watch says nothing of it.
        if self.opts.watcher.is_none() {
            return;
        }
        for &(folder, watch) in watches {
            if let Watch::Watched(wd) = watch {
                // The folder's number held another descriptor: a number
                // given again. That one's watch goes.
                if let Some(old) = self.by_folder.insert(folder, wd)
                    && old != wd
                {
                    self.watches.remove(&old);
                    if let Some(watcher) = &self.opts.watcher {
                        watcher.unwatch(old);
                    }
                }
                // The descriptor watched another folder: the same folder,
                // moved and read again under a number of its own.
                if let Some(other) = self.watches.insert(wd, folder)
                    && other != folder
                    && self.by_folder.get(&other) == Some(&wd)
                {
                    self.by_folder.remove(&other);
                }
                continue;
            }
            let path = self.index.path_of(folder);
            for live in &mut self.live {
                if path.starts_with(&live.scope) {
                    live.short = Some(watch);
                    live.since = None;
                }
            }
        }
    }

    /// Let go of the watches of folders no longer in the index.
    fn prune(&mut self) {
        let tree = self.index.tree();
        let_go(
            &mut self.watches,
            &mut self.by_folder,
            self.opts.watcher.as_deref(),
            |folder| tree.loc(folder).is_none(),
        );
    }

    /// Let go of the watches of folders whose path `drop` names.
    fn unwatch_where(&mut self, drop: impl Fn(&Path) -> bool) {
        let index = &self.index;
        let_go(
            &mut self.watches,
            &mut self.by_folder,
            self.opts.watcher.as_deref(),
            |folder| drop(&index.path_of(folder)),
        );
    }

    /// The kernel let go of this descriptor's watch: its folder removed,
    /// or its filesystem unmounted. Its folder is read again from its
    /// parent's events, or the next check.
    pub fn forget(&mut self, wd: i32) {
        if let Some(folder) = self.watches.remove(&wd)
            && self.by_folder.get(&folder) == Some(&wd)
        {
            self.by_folder.remove(&folder);
        }
    }

    /// The live subtrees, put in the index for its snapshots to answer.
    fn publish_live(&mut self) {
        let live = self
            .live
            .iter()
            .filter_map(|l| l.since.map(|t| (l.scope.clone(), t)))
            .collect();
        self.index.set_live(live);
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
        // Nothing new since the last save: nothing written, not even the
        // manifest (DESIGN 8, "A save writes what is new"). The proof saved
        // then stays older, which claims less.
        if !self.unsaved() {
            return Ok(());
        }
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
        self.saved = self.log.as_ref().map(|log| (log.epoch(), log.next() - 1));
        Ok(())
    }

    /// Whether anything is new since the last save: a record logged, a
    /// chunk not read from its file, a name not in one, or no save yet.
    fn unsaved(&self) -> bool {
        let at = self.log.as_ref().map(|log| (log.epoch(), log.next() - 1));
        at.is_none()
            || at != self.saved
            || self
                .index
                .tree()
                .chunks()
                .any(|(_, chunk)| !chunk.is_mapped())
            || self.index.names().unsaved()
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
        self.purged()
    }

    /// Folders let go of in memory, let go of on disk: their names
    /// dropped, the log that held them ended, the index saved.
    fn purged(&mut self) -> std::io::Result<()> {
        self.index.blank_dead_names();
        self.new_epoch()?;
        self.save()
    }

    /// How large its chunks grow.
    pub fn limits(&self) -> Limits {
        self.limits
    }
}

/// Let go of the watches of the folders `drop` names, from both maps.
fn let_go(
    by_wd: &mut HashMap<i32, FolderId>,
    by_folder: &mut HashMap<FolderId, i32>,
    watcher: Option<&skwird_crawl::Watcher>,
    drop: impl Fn(FolderId) -> bool,
) {
    by_wd.retain(|&wd, &mut folder| {
        let keep = !drop(folder);
        if !keep {
            if by_folder.get(&folder) == Some(&wd) {
                by_folder.remove(&folder);
            }
            if let Some(watcher) = watcher {
                watcher.unwatch(wd);
            }
        }
        keep
    });
}
