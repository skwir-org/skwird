# skwird's design

Status: design, 2026-10-10, revised on 2026-10-11 by phase 0's numbers,
on 2026-10-20 by what fsearch and its Linux port changed since, and on
2026-11-21 by the choices made before phase 1 (2026-11-11) and what
skwir decided up to its 0.6.0. Only `skwird-bench` (phase 0,
`docs/PLAN.md`) is built. Each decision says why, so that it can be
revisited knowingly.

## What skwird is

An index of the names of every file under the folders it covers, for
Linux, macOS and Windows. It is kept current by the system's change
notifications and answered over a local socket in milliseconds. Every
answer says how far it can be trusted.

It is a general tool: a command line, and a protocol any program can
speak (file managers, launchers, editors). skwir, a file manager, is
its first client, and the reason several decisions below exist.

It borrows from two MIT-licensed projects, with credit where code is
taken:

- [fsearch](https://github.com/noahdunnagan/fsearch) (macOS):
  - the index layout (8);
  - changes kept over a base, which skwird keeps as chunks rewritten
    whole (8);
  - FSEvents replay, of events the system dropped as well.
- [fsearch-crossplatform](https://github.com/threehymns/fsearch-crossplatform):
  - a Linux walker with `getdents64`;
  - inotify watching per folder.

The bugs both fixed are lessons too, recorded below where they apply.
Both were last read on 2026-11-21, at fsearch 0.2.2 (`4bc11ce`) and
fsearch-crossplatform `8857eec`, which has not changed since
2026-10-12.

It does none of these:

- **Search inside files,** ever (open question 4, answered). Content,
  and what is derived from it, is built per user by the user's own
  client, as skwir's harvest builds it (skwir's ADR-089). An engine that
  reads every user's files, as the system service does, keeps names and
  attributes only.
- **Types by extension.** Clients type files; skwir uses the desktop's
  MIME database.
- **A query language in the protocol.** Queries are data; the command
  line's syntax compiles to them.
- **A user interface** beyond the command line.
- **The network.** Never.

## Decisions

### 1. One engine, in a process of its own, with three lifetimes

| Started by | Lives | Covers |
|---|---|---|
| The system at boot (a system service) | Always | Every user's files; answers filtered for each asker (4) |
| Login (a systemd user unit, a LaunchAgent, a Windows logon task) | The session | The user's files |
| Its first client | While a client is connected, then a minute | The user's files |

How clients find an engine, in this order:

1. the system service's socket;
2. the user's socket;
3. otherwise the client starts `skwird` itself if it is installed;
4. otherwise it does without (a client must work without skwird). It
   does without an engine that speaks no protocol version it speaks,
   too, and says so.

There is one engine per scope: one for the system, one per user, held
by a lock. A second engine blocks on the lock and takes over when the
first exits, even during its first crawl; it does not poll the lock, as
fsearch's followers do every 10 seconds (10). A user's engine that
finds the system service covering its roots live (2) leaves them to it.

**What the system service covers** (the user's choice, 2026-11-22):
every local filesystem that stores files, except the Nix store
(`/nix/store`): millions of read-only entries, changed by nix's builds
alone, whose names nix answers for itself. Never covered, whatever is
configured:
- the kernel's own filesystems (proc, sysfs, cgroup, devpts and the
  like) and memory ones (tmpfs, ramfs), whose names are the running
  system's state, not files kept;
- network mounts (2);
- removable volumes, until open question 8 is answered: not covered,
  removable;
- btrfs snapshots (read-only subvolumes) below a root: left out, as
  excluded, their names kept (open question 10).

A configuration file names other exclusions, as a user's does (5).

**As built** (phase 3, step 4): `skwird serve --system` finds its roots
in the mount table, unless named (`--root`). Each is the point of a
mount of a filesystem that stores files (ext2 to 4, btrfs, xfs, f2fs,
bcachefs, zfs, jfs, reiserfs, nilfs2, vfat, exfat, ntfs, hfsplus), not
removable (a disk the kernel says is, or one on a USB bus), not
reaching a network, not the Nix store, and shown once: a bind mount of
what another mount shows already is left out. Its folders are its
service manager's (`RUNTIME_DIRECTORY`, `CACHE_DIRECTORY`,
`STATE_DIRECTORY`, `CONFIGURATION_DIRECTORY`) or `/run/skwird`,
`/var/cache/skwird`, `/var/lib/skwird` and `/etc/skwird`; its socket
is open to every user (the folder 0755, the socket 0666), its index and
state its own (0700). A filesystem that stores files, mounted while it
runs, is a root from then on, marked, read and live as at the start;
one unmounted is let go of, and a path there answered by the root
holding it now (roots found in the mount table alone, not named). A
query or a coverage at a folder above
several roots (`/` above `/home`) is answered from each, every root
below at its place: its mount hole gone, the proof the weaker of all;
one the asker may not reach, or not read yet, a hole there.

An engine a client starts runs in a process group of its own, so that a
terminal's interrupt does not reach it, but in the client's control
group. Where a service manager runs the client, the engine ends with the
client's run: skwir's `skwir index`, run hourly by a systemd timer
(skwir's ADR-089), takes its engine down with it, and the next client
starts another. On the home phase 1 measured (6.9 million entries), that
one reads its index back and answers in 0.6 s, as of its last save, and
a sync proves it current in 0.65 s, warm. So every save survives a kill,
and the user's unit (phase 2) is what outlives such runs.

The engine's crates can be embedded by other programs. Clients are
supported through the protocol only, for four reasons:

- **One code path.** There is one protocol to test, not a library mode
  and a socket mode that must behave alike.
- **One set of watches per scope.** In fsearch-crossplatform, an
  application linking the engine follows the daemon's index but
  installs its own watches: one inotify watch per folder per process,
  installed again each time the daemon saves its index. Its README
  counts 465k watches for one developer's home, against a limit of
  524k, so two processes do not fit.
- **Clients stay light.** The index's memory, its crawls and its calls
  into the system (fanotify, FSEvents, the change journal) stay out of
  their processes, and a crawl stuck on a sleeping disk stalls no
  window.
- **Isolation.** A crash or a hang in the engine is not the client's.

A client takes `skwird-client` and `skwird-protocol` as copies in its
own tree, or speaks the protocol with code of its own (Protocol,
below).

### 2. Every answer says how far it can be trusted

This is skwird's core, not search. An answer that says only "these
match" is a hint: a careful client still walks the tree to be sure,
and gains only its first results. An answer whose coverage is proven
lets the client skip the walk.

For each root a query names, and for each kind of fact, the answer
says one of:

- **Live since T**: every change was delivered, with nothing dropped,
  since a full sync at T, and applied up to a barrier the client set.
- **Checked at T**: every folder was compared with the index at T.
- **As of T**: a snapshot, such as an index just loaded, or plocate's
  database.
- **Not covered**, and why: outside the engine's roots, excluded,
  private (a `.private` file), permission denied, a read that failed or
  was cut short, gone or replaced since its parent was read, a mount
  below the root, a network mount, not read yet, a watch limit reached.

The kinds of fact are:
- **names** (and kinds: file, folder, link, other);
- **attributes** (identity, size, times);
- **content**, later.

Coverage is never overstated. An overflow, a failed watch, a mount
appearing or a folder denied lowers it at once, for the subtree
concerned, until that subtree is read again.

- **A failed read is not an empty folder.** A folder that cannot be
  read, or whose listing fails part-way, is not covered, and what the
  index held under it stays, as of when it was read. fsearch and its
  port both did otherwise: a folder refused with `EACCES` was indexed as
  empty, and a listing cut short by an error was taken as complete.
  What is let go of at once is what the folder no longer shows as its
  own: a folder made private or excluded (its names are never kept), and
  a folder a mount now covers (the names there are the mount's).
- **A dropped event ends "live"** until the replay or the reread is
  done. Where the system keeps a history (FSEvents, the change journal),
  the position kept to resume from is never past an event not yet
  applied. That includes a recovery put off, a relist waiting for the
  disk to calm, which fsearch let the position pass (its `052f636`).
- **Only the current source proves anything.** A change source started
  again (an inotify instance made again, fanotify marked again, an
  FSEvents stream replaced) gets a generation. Events still queued from
  an older one are applied, but prove nothing: only the current
  generation's markers may move the position kept or bring coverage
  back. Those markers are the end of a replay, a sync's cookie heard,
  and a recovery's end. fsearch let an old stream's events move the new
  stream's position, and its "history done" end the new stream's replay
  early (its `61f8c8d`, `d78a4fc`).
- **A position kept is used only where the history still holds it.**
  The history must be the same one (its identity: the change journal's
  id; on macOS, the event database's, to be verified in phase 5), and
  the position must not be past the history's current end, nor before
  what it still keeps (the journal's lowest valid USN). Otherwise the
  engine runs the check and answers "as of" until it is done. macOS
  26.3 numbered its events afresh, below fsearch's saved id. A stream
  started from that id then replayed nothing, and said nothing (its
  `2919777`).
- **The barrier** makes "live" mean up to now. The client asks for a
  sync. skwird creates a cookie file in a folder of its own that shares
  the notification queue, and answers once it hears it. Where the
  system has a flush (`FSEventStreamFlushSync`), that is used instead.
  Watchman and git's fsmonitor daemon work the same way. A rename the
  client just made is then in the answer. Where nothing watches the
  root (a user's engine before phase 2, or a folder beyond its
  watches), the sync runs the check on that root and answers "checked
  at" a time after it was asked. skwir needs this after its own plans:
  its lenses ask no index again after a plan, since the index learns of
  the plan's changes after it (skwir's ADR-126).
- **The check** proves names current without any watch. A folder's
  change time moves whenever its entries change. So comparing each
  folder's change time with the one stored, for equality, finds every
  folder to read again. Three rules keep this sound:
  - The time stored is read before the folder's entries, so a change
    during the read leaves a time that differs.
  - Change times can be coarse: a second change within one tick of the
    clock can leave the time as it was, so a folder read within a tick
    of its last change is read again at the next check, as git does
    with its "racily clean" entries. The tick is two seconds, the
    coarsest change times local filesystems keep; a change time ahead
    of the clock counts as within it.
  - The folder's device and inode are compared too. The check goes by
    path, which phase 0 found fastest, and a path follows a link in any
    of its folders (`O_NOFOLLOW` guards only the last name). A folder
    replaced, or reached through a link put in place of a folder above
    it, is then another folder, and is read again. A folder is read
    through its parent's handle, or by a path that may hold no link
    (`openat2` with `RESOLVE_NO_SYMLINKS` on Linux, `O_NOFOLLOW_ANY` on
    macOS), so another folder's names are never indexed under its path.
    On Linux that path is also beneath the root and crosses no mount
    (`RESOLVE_BENEATH`, `RESOLVE_NO_XDEV`), and one longer than the
    system resolves at once is opened a part at a time. A path longer
    than the system allows is checked through its parent's handle as
    well. The crawl needs Linux 5.8 (`openat2`, `statx`'s mount id), and
    refuses an older kernel, saying what it lacks.

  It is not a comparison against a timestamp with a margin, as fsearch
  does with modification times (every folder changed since 120 seconds
  before its last sync), and its port with it: `tar` and `rsync -a` set
  those back after filling a folder, and cannot set a change time. A
  margin also depends on the time it is counted from: fsearch 0.2.2
  fixed replayed events stamped with their arrival, which moved its
  sync time ahead, so its catch-up looked at too little (its `182a964`,
  `55d8845`). An equality has no time to count from. This
  holds only where change times tell: local filesystems, not network
  shares or FUSE. It proves names, not attributes: a file's size changes
  no folder's time.
- **Watch, then read.** A folder is watched before it is read or
  checked, so a change between the two is heard. fsearch-crossplatform
  catches up first and watches after, and misses what changes in
  between.
- **Answers are exact for covered facts under covered roots.** Clients
  still check what they show (a stat), so a stale result costs one
  check. The guarantee is about results missing.
- **The index reaches what a walk reaches.** Hidden names are indexed
  (clients filter them), and so are tool stores (`node_modules`,
  `~/.cargo`, caches): a client that leaves them out of its searches
  filters skwird's answers, as skwir does by its ADR-125 (the user's
  choice, 2026-11-11, to revisit if phase 1 measures the index too
  large). Links are never followed, and each mount below a root is
  reported, covered or not. A mount is told by its mount id
  (`statx`, Linux 5.8), not by its device: each btrfs subvolume has a
  device of its own and is no mount, and a bind mount keeps its
  filesystem's device. fsearch-crossplatform tells mounts by device, so
  it skips subvolumes and walks through bind mounts. The crawl, the
  check and a reread share one test, which holds at the mount's own
  folder as well: fsearch once crawled a whole volume when a change was
  reported on its mount point.
- **Mounts come from the mount table,** never from reading a path: a
  stat of a dead server's mount point blocks. The table is read again
  at each crawl and check, and followed from phase 2, as skwir follows
  it (`POLLPRI` on `/proc/self/mountinfo`). The mount id is the backstop
  for a mount made since the table was read.
- **Network mounts are never crawled** (the user's choice, 2026-11-11).
  A network mount below a root, or a root on one, is not covered:
  network. Requests would leave the machine (rule 6), and a share's
  change times do not tell anyway. A mount is told as skwir tells it
  (`skwir_core::fs::reach::network`): one of the kernel's network
  filesystems, a FUSE subtype known to reach one, or a FUSE source that
  names a server. skwir's walks and indexes never enter one either (its
  `docs/invariants/remote.md`). Removable volumes: open question 8.

### 3. Privileges are detected, and become coverage

There is no "root mode". Whatever starts skwird grants it capabilities:
systemd's `AmbientCapabilities`, launchd, a Windows service account.
skwird uses what it was given and reports the coverage that results.

| | As a user | With privileges |
|---|---|---|
| Linux | `getdents64` walk. inotify needs one watch per folder, about 1 KB of kernel memory each, within `max_user_watches`. No history across restarts: the check (2) catches up | fanotify on whole filesystems (`FAN_MARK_FILESYSTEM`, `FAN_REPORT_DFID_NAME`: Linux 5.9 or later, `CAP_SYS_ADMIN`). No watch per folder, every user's files. Still no history across a reboot. On btrfs, mark through a detached read-only mount of the top-level subvolume (below) |
| macOS | `getattrlistbulk` walk. FSEvents keeps a history on disk, so a restart replays what changed | Root adds little; Full Disk Access decides what can be read |
| Windows | `FindFirstFileEx` walk. `ReadDirectoryChangesW` watches a whole tree with one handle. No history | The volume's file table to crawl and its change journal (USN) to catch up after a restart, as Everything does. Needs administrator rights and NTFS |

As a user on Linux, the check is the base mechanism, and inotify watches
are a bounded extra for the folders that change most (open question 2).
Phase 0 measured why, on a home of 5.8 million entries in 652,336
folders. A watch per folder would take more than the user's whole limit
of 524,288 watches. The check proved every folder current in 1.9 s from
a cold cache and in 52 ms warm, where a walk took 10 to 13 s cold and 1
to 2.7 s warm.

On btrfs, a mark of the whole filesystem through a subvolume is refused
(`EXDEV`): a subvolume's filesystem id is not its superblock's. The
service then marks through the top-level subvolume (id 5), mounted
read-only and detached, attached to no folder. Phase 0 tried it on
Linux 6.18.55. The mark was accepted. Three probes, each in a subvolume
with a filesystem id of its own, made, renamed and removed a file, and
all nine changes were heard. The events came with the mark's
filesystem id, not the subvolume's, so an event's filesystem id cannot
tell its subvolume: the service keeps each event's folder handle and
opens it through the detached mount. Every handle opened to its own
folder, by device and inode.

That shows the way works, on one kernel and one filesystem; it does not
prove live coverage. A mount, a mark or a handle that fails lowers
coverage, and phase 3 tests an overflow, a mount or subvolume appearing,
and a handle that does not open.

**As built** (phase 3, step 3, 2026-11-22: `skwird_crawl::fanotify`,
proved as root in the test machine on Linux 6.18.58):
- **One mark a filesystem,** known by the device the mount table gives
  it, which every subvolume of one btrfs shares: a btrfs mounted
  through a subvolume was marked through its top level once, and a
  change made in each of three subvolumes was heard.
- **The mount made for the mark is let go of at once.** The mark is the
  superblock's and outlives it: the changes above were made after it
  was gone.
- **Nothing of a filesystem is held open.** A descriptor kept on a mount
  would make its user's `umount` fail, and then a `cryptsetup close`
  run after it. Handles are opened through a mount of the filesystem,
  found in the mount table when they are read, and known to be that
  mount by its mount id: a mount put over its point since is not taken
  for it, and the handle fails to open rather than open to another
  folder. A root's own folder is held open only while it is read, and
  opened again after, known by its identity and mount id (step 4): its
  mount gone, the root is not covered, never read as the folder under it.
  A test unmounts a root between two reads.
- **An event names its folder by handle;** opened through a mount of
  its filesystem, the handle gives the folder's device and inode, as the
  index keeps them, and its path through that mount. The path is taken
  only if, stat-ed again following no link, it is that folder; each
  mount is tried in turn, and a folder none reaches (in a btrfs
  subvolume mounted nowhere) is unreachable, in no root. The engine
  finds the folder in the root holding the path, by its path and known
  by its identity, so that a folder put in its place is not taken for
  it; one the index does not hold there yet is brought in by its
  parent's event. Each handle heard opened to its folder, in its own
  subvolume, at its path.
- **A handle that no longer opens (`ESTALE`) is a folder gone,** and
  proves nothing lost. A folder is emptied before it is removed, and
  the removal of the topmost folder removed is heard in its parent,
  which is still there, after what was heard in the folder. Any other
  failure to open a handle proves nothing heard on that filesystem.
- **A folder's own attributes changed** (a `chmod`) are heard as that
  folder changed, by its own handle, and a folder moved in both its
  parents.
- **Two filesystems with one id** are not both marked: their events
  could not be told apart. A filesystem that cannot give handles
  (`ramfs`) refuses the mark, and says so.
- **The queue holds 16,384 events:** more, unread, are heard as an
  overflow.

### 4. A system service answers only what the asker could list

- **Who asks** comes from the transport: `SO_PEERCRED` and
  `SO_PEERGROUPS` on Linux (the asker's user and every group, as the
  kernel holds them when it connected, with no lookup in the user
  database), `getpeereid` on macOS, impersonating the named pipe's
  client on Windows.
- **A result is given only if the asker could list its folder and enter
  every folder above it.**
  - The index keeps each folder's owner, group and mode, and its access
    ACL where it has one.
  - The service evaluates them itself (the user's choice, 2026-11-22),
    against the asker's user and groups, in the kernel's order (acl(5)):
    the owner; the named users; the owning and named groups, under the
    mask; the others. An ACL that cannot be read leaves the folder
    unanswered, never answered as its mode says.
  - No thread takes the asker's ids, as this decision first had it
    (`setfsuid`): that needs `CAP_SETUID` and `CAP_SETGID`, beyond the
    two below, and gains nothing over the evaluation. A security module
    (SELinux, AppArmor, Smack) judges the service's own context, not
    the asker's, either way: its rules for users are not applied. On a
    system whose security module narrows what a user may list beyond
    modes and ACLs, the service answers what the modes and ACLs allow.
    On Windows, the check is made while impersonating.
  - Each folder's answer is kept for each asker until the folder
    changes. plocate checks every parent folder of a result for the same
    reason.
- **The system index is readable by the service alone.** Only a user's
  own index may be mapped by that user's clients.
- **Limits per asker:** queries at once, results, time. Every query can
  be cancelled.
- **Sandboxed:**
  - a user of its own, with only the capabilities it needs:
    `CAP_SYS_ADMIN` for fanotify (broad, the price of fanotify) and
    `CAP_DAC_READ_SEARCH` to read every folder;
  - `ProtectSystem=strict`, `NoNewPrivileges`, `PrivateNetwork` and a
    system call filter.

  It reads every name on the machine and answers any local user, so it
  is treated as attack surface from the first line.

  **As built** (step 7, `dist/systemd/skwird-system.service`): a static
  user, `skwird` (systemd-sysusers, `dist/sysusers/skwird.conf`), with
  `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH` alone, ambient and bounding;
  `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp`,
  `PrivateNetwork`, `PrivateDevices`, AF_UNIX alone, the kernel's
  tunables, modules, logs, clock and control groups protected, no new
  namespaces, and `@system-service`'s calls with the few it needs beyond
  (`openat2`, fanotify, handles, the mount calls). Not `DynamicUser`,
  which implies `RestrictSUIDSGID`, which refuses `openat2` whole
  (systemd cannot read its flags, behind a pointer): the crawl needs it
  to follow no link. Its index and state are its own (0700), its socket
  every user's (0666, its folder 0755).

**As built** (phase 3, step 5, 2026-11-22: `skwird::asker`):
- Root, and a user asking their own engine, see the index whole. Any
  other user asking the system service is answered a result only if
  they may read and search its folder and search every folder above it,
  up to the filesystem's own top: within the index by its folders'
  facts, above the root as the index of the root holding each folder
  says now, live (as it was at the start where no root holds it: a
  review found a folder closed since left what was below open). A
  folder they may read but not search holds names they are
  not given (`ls` would list them, and stat none).
- Each folder is judged once a query, with what is above it.
- **ACLs are evaluated** (step 5b, `skwird_crawl::acl`). The crawl says
  which folders have an access ACL (`skwird_crawl::ACL` in their mode,
  one `fgetxattr` a folder); the keeper reads each such ACL through the
  root, following no link, taken only if the folder there is the one the
  index holds, and keeps it by the folder's identity until its change
  time moves. The ACLs travel with each snapshot, so that answering
  reads none. An asker is judged by a folder's ACL as acl(5) says: the
  owner by the owner's entry; a named user within the mask; a member of
  the owning or a named group by any of their entries that grants it,
  within the mask, never falling to the others'; anyone else by the
  others'. An ACL that cannot be read leaves its folder not known: a
  result in it is withheld, and the topmost such folder whose parent
  the asker may list is named not covered (denied), so that the client
  walks it. After what was heard, the folders read again and the
  folders they hold have their ACLs read; a folder with an ACL deeper in
  a subtree just made is not known until the next check. A folder above
  a root with an ACL is not known either.
- **A hole is a folder's name,** said only to who may list the folder
  holding it, as its listing would (a review of 2026-11-22 found every
  hole given to every asker: another user's private and excluded folders
  by name). A folder whose ACL was not read is a hole whether or not
  anything in it answered, so that whether a name is there is not said.
- **Whether a folder exists is said only to who could list its
  parent:** a coverage or a query at a folder the asker may not list,
  or at a path below one, is answered not covered (denied) alike.
- **Limits per asker** (2026-11-22): a query of the system service
  gives at most 100,000 results, past which its `done` says there were
  more; a user runs at most four queries at once, a fifth refused, busy,
  and holds at most 64 connections open, each a thread of the service's,
  the next closed; a user other than root asks the writer for work (a
  sync, a registration, the journal) once a second at most, the next
  refused, busy. Each request is a line of bounded length. Not yet: a
  time limit, and the answers kept per asker
  across queries. The journal is filtered for each asker (Protocol,
  `changes`).

### 5. Private folders are never indexed

It is not enough to leave them out of answers: they are never read into
the index. skwird honours what marks them itself:
- a `.private` file in a folder (skwir's convention);
- default exclusions: `~/.ssh`, `~/.gnupg`, the keyrings
  (`~/.local/share/keyrings`, `~/.local/share/kwalletd`), and the
  password store (`~/.password-store`), whose names are the accounts it
  holds;
- a user's own exclusions, in skwird's configuration;
- folders a client registers (the user's choice, 2026-11-11);
- the engine's own folders: its index and its log, its state, its
  socket and cookies (phase 2). Within its home, a user's engine that
  indexed and watched them would hear its own saves, and read itself
  again without end: a trial of phase 2 found it taking a tenth of a
  core, idle.

**A client narrows what is indexed, never widens it.** It registers
folders over the socket, with its user's credentials, and skwird keeps
them for that user, across restarts, until that client withdraws them.
A folder registered after it was indexed is purged: its entries, and
the log's entries under it. An answer says such a folder is not
covered: excluded, so the client walks it when its user searches there.
skwir's ADR-125 keeps its secrets from agents and from its index, not
from the user's own searches.

A registration names folders, never patterns. skwir's Secrets category
(ADR-125) mixes folders (`~/.ssh`, password managers' and browsers'
profiles) with names and globs that hold anywhere (`.env`, key files).
skwir registers the folders it resolves. It keeps filtering the rest
from its agents itself, as it filters tool stores (2): such a file's
name is in its user's own index, never its content (ADR-089).

The same registration answers how a system service learns a user's
exclusions (open question 3). It applies only to folders the
registering user owns, so no user can take another's folders out of
the index.

**The system service reads each user's own files too** (the user's
choice, 2026-11-22), so that a user's private folders are never indexed,
even by a first crawl before their client connects: their exclusions
(`~/.config/skwird/exclude`) and what their clients registered with
their own engine (`~/.local/state/skwird/registered`), at the default
places, since the service knows no user's environment. It reads them
through the home's handle, following no link, only if the user (or
root) owns them and no other user can write them, as sshd reads a
user's keys. Each folder they name applies only if that user owns it.
The default exclusions hold in every home the user database names.
What the user's clients register with the service itself is kept for
them by the service.

**As built** (phase 3, step 6, 2026-11-22): the users are the user
database's entries (`/etc/passwd`) and each folder in `/home` by its
owner. A user's two files are opened through their home's handle,
following no link and staying beneath it (`openat2`), and taken only if
they are files the user or root owns that no other user may write
(group and others' write bits clear); a mebibyte at most. Each folder
they name, and each a user registers with the service (kept with their
user id), is resolved, then kept only if the folder it resolves to is
still that folder (its identity) and the user owns it, or, where none is
there yet, if it is in a home the user owns; root owns every folder.
They are derived again with every other exclusion: at the service's
start, at each sync and at each registration. A user whose files are
not at the default places has what their clients register.

The default exclusions, at the system service, hold at their own place
in each home, no link in their way, whoever owns them; reached through
a link, only if the user owns what it leads to (the review found a user
could link `~/.gnupg` to another's folder and take it out of everyone's
answers). A user's file may be writable by their group if that group is
their own private one (their primary group, no one else's, naming no
other member), as umask 002 leaves it with a group of each user's own;
a file not taken is counted in the log. A registration refused for a
folder not the user's is not kept, nor said kept, and a user keeps at
most 256 at the service.

An exclusion, like a root, is resolved once to the folder it names, and
compared as that folder (its identity, or its range in the index),
never as the spelling of a path. In fsearch, an exclusion written with a
trailing slash excluded nothing, an empty one excluded everything, and
`/h/m` was taken to hold `/h/me`. In skwird the first would index a
private folder. So the path is named as the system resolves it (links
followed, doubled and trailing slashes dropped), the folder there is
kept by its identity, and the resolved path is compared by whole
components. One that names no folder yet is kept by its resolved path,
and applies to the folder made there. An empty or relative path is
refused, never taken to mean everything.

**The folders an index holds as excluded are always those the
exclusions name** (found on 2026-11-21). They are derived again from
the list whenever either side changes, never kept up by the events
that changed them alone:
- when an index is opened, before anything reads it;
- when a client registers or withdraws a folder;
- at each `sync`, the user's file read again.

A folder the list names is purged; one it no longer names is not
covered, not read yet, until the next refresh reads it. Otherwise an
exclusion would hold only once its folder's parent was next read. A
line added to the user's file, while an engine ran or while none did,
left the folder's names indexed and answered. A purge or a readmission
cut short by a kill was lost. fsearch lost a recovery so (its
`705c5eb`), and its review led here.

### 6. Answers carry identity

Each entry comes with its device and inode, and its folder's. That lets
a client:
- tell a file from its names (hard links);
- follow a rename;
- keep what belongs to the file by the file (skwir's ADR-049).

The cost is 8 bytes per entry. The device is kept once per subvolume,
for its range of the index, not once per filesystem: each btrfs
subvolume has a device of its own and numbers its inodes from the same
start (every subvolume's top folder is inode 256), so a filesystem's
device and an inode do not name one file.

### 7. The engine knows names, not types; queries are data

- **Facts:**
  - name and kind;
  - identity;
  - size and allocated size;
  - modification and change times (instants);
  - for folders, owner, group and mode.
- **No types.** Clients type files.
- **Queries are structured:**
  - name predicates with defined meanings (exact, prefix, suffix,
    substring, glob), and fuzzy ranking for interactive use;
  - a root, which is one range of the index;
  - kinds;
  - ranges of size and of time. Times are instants, so a client
    computes calendar boundaries in its user's time zone.
- **Names match as skwir matches them,** in protocol 0, so an answer
  holds whatever skwir's query plugin would match (its `NamePat`):
  - a substring, after `str::to_lowercase` of both sides, or exactly
    when the case is asked to count;
  - a glob, as the `globset` crate compiles it, case-insensitive unless
    asked, with no separator rule (a name holds none);
  - both on the name's lossy UTF-8 form. A name that is not UTF-8 is
    still answered, and travels byte for byte.

  Normalization and fuzzy scoring stay open (question 5).
- **The command line** has a text syntax of its own, compiled to the
  same queries.
- **Answers may be wider, never narrower.** An answer may hold more
  than a client's own predicate would (a client with stricter rules
  filters), never fewer under covered roots.

### 8. The index

fsearch's layout, with what skwird needs on top.

From fsearch (0.2):
- a file mapped into memory and read in place;
- entries in folder blocks, depth first, so a folder's whole subtree is
  one range;
- each distinct name stored once, so a query scores names, not entries.
  Which names can match is answered 64 names at a time, from bitmaps of
  what each name holds (classes of letters, the starts of words), before
  any name is read;
- each array packed at the bits its largest value needs, and sizes and
  times in runs of 64 from a base;
- zones: for each 1,024 entries, the largest size, the range of times
  and the kinds present, so a query on size or time skips most of the
  index.

fsearch also keeps a base and an overlay of changes with tombstones.
skwird does without the overlay (below, "How a subtree stays one
range"). fsearch 0.2.2 measured what its overlay costs: about 190
bytes an entry, plus its path twice. Its old bound let the overlay
reach about 1.7 GB over an index of 9.3 million entries, and it is now
capped (its `c288c93`).

skwird adds:
- one file per filesystem, where fsearch keeps one for all and does not
  cross a mount;
- each entry's inode;
- each folder's change time, owner, group, mode and ACL flag;
- each filesystem's identity (its UUID where the system gives one), so
  a removable drive's index survives being unplugged;
- changes made by folder, never by path: a folder's subtree is one
  range, so removing a folder removes everything under it. fsearch's
  tables keyed by path kept a removed folder's new files searchable,
  until 0.2.1;
- a change rewrites the chunks it touches, never the whole index.
  fsearch rewrites everything, about 280 MB by its own estimate, and
  its port does the same. Since 0.2.2 it does so:
  - past an eighth of its base in changes (at least 50,000, at most
    175,000), once the disk has been quiet for 30 seconds;
  - past 350,000 changes at any time;
  - after 12 hours, once quiet.

**Changes heard are gathered, a folder at a time** (decided on
2026-11-21 from phase 2's step 2). A folder's events are gathered until
100 ms pass without one, or 1 s after the first. Then the folder is
read again once, and its changes logged once. A sync applies what is
gathered below the path it asks at once, so a live answer after a sync
holds every change made before it. Without a sync, an answer lags what
was heard by a second at most, but for a folder whose reads would
take more than their share of a core (below).

The measure was a full build of skwird's own workspace, watched, into a
scratch folder (9.7 s, 15,000 events). Applied at once, it read folders
again 7,462 times, 770 a second, each read stating every entry of a
build's large folders, and logged 3.0 MB: the journal's 16 MB would
last 55 s of such building. Gathered at 100 ms and 1 s, it read 269
times and logged 688 KB. fsearch's batches of busy folders (5 s of
quiet, 60 s at most) halved that again, with answers up to a minute
behind.

So no class of busy folders is kept: every watched folder gathers
alike, and no set of folders is learned beside the index. A build that
lasts long enough still fills the journal: its clients' tokens turn
"too old", and they walk again. That is the journal's answer, not a
loss.

**A folder read again stats what was named** (decided on 2026-11-21,
from the user's home). Its listing is read whole, so its names are
every name it holds. Of its entries, those its events named are
stat-ed, and those new to it; every other one keeps what it was read
with, if the listing gives it the same inode and kind. An entry's size
and times change only through an event naming it. A folder heard of
itself, or by more than 4,096 names, is read whole. Watching the user's
home, one cache folder of 62,000 entries was heard about once a
second, and each read stat-ed all of them: 60,000 stats a second, 9%
of a core. In a folder of 2,000 entries, a file written to is now read
again with at most two stats.

**Applying what is heard keeps to a share of a core** (decided on
2026-11-21, from the user's home). A read of a large folder still
costs every entry its listing holds, a microsecond or less each: the
listing, what was kept, the merge. Without its listing a folder's names
would rest on its events alone. So, after each application, each
folder read waits, before it is applied again, for its share of the
processor time the application took, by its entries, 49 times over.
That time is the writer thread's own (`CLOCK_THREAD_CPUTIME_ID`), not
the clock's, so a busy disk's waits hold nothing. Applying takes at
most 2% of a core; a large folder heard without pause waits for
itself alone; a sync applies it at once. On a synthetic tree, a folder
of 62,000 entries written every 100 ms cost 1.4% of a core, and a
change in a small folder meanwhile was answered in 122 to 282 ms.

**How a subtree stays one range** (the format's spike, 2026-11-21).
The blocks are in pre-order, each with its depth: a folder's subtree is
its block and every block after it that is deeper, so no block stores
where its subtree ends, and no change writes a folder above the one it
changes. The blocks are cut into chunks of about 16k entries (a chunk
is cut past 24k, but for a single block; one below 4k joins a
neighbour that can take it in). A change keeps pre-order:
- a folder made is inserted right after its parent's own block, which
  pre-order allows, so only that chunk is rewritten;
- a folder removed takes its range with it;
- a folder moved is its range taken out and inserted after its new
  parent's block, every depth in it moved alike.

A folder keeps its number through all this. The folder table, which
says where each folder's block is now, is rebuilt from the chunks
when the index is loaded, and kept in pages that a snapshot shares. A
chunk being small, a change rewrites it in memory at once, copy-on-
write, for the next snapshot: that is the overlay, and the log keeps it
durable until the chunk is written. The spike held the promise through
240,000 random edits (files made and removed; folders made, removed
and moved; nested ones), every folder's range compared with a model
tree after each, with chunks of a few dozen entries so that every edit
cut or joined some.

**A chunk's size is a folder read's price** (decided on 2026-11-21, by
phase 2's live measurement). A change rewrites its whole chunk, however
small the change, so a folder read again costs what its chunk holds.
With chunks of 64k entries, a folder of 100 entries read again cost
3.6 ms, changed or not. Watching the user's home as far as its share
went, the engine took 7.5% of a core while no client asked, its busy
folders read again over and over. Two changes:
- A folder read back as it was is not written again: 97 µs.
- Chunks of 16k entries make a read that changes something cost
  1.1 ms. 4k entries would make it 0.4 ms, but on a synthetic tree of
  a million entries, loading was 49% slower at 16k and 69% at 8k (38%
  at 4k: one run each, noisy). The user's home loaded in 0.6 s with
  64k chunks, against a budget of about 1 s.

An index saved with larger chunks is cut down to the new size a chunk
at a time, as changes rewrite them.

**The log is the journal.** Every change the writer applies, whatever
found it (the first crawl, a check, an event), is appended to a
numbered log before it reaches a chunk. One structure does three
jobs: saves are incremental, a crash is replayed from it, and it
answers `changes {root, since}`, which skwir's harvest consumes (its
ADR-089). Its entries carry each entry's identity and kind. A token is
an epoch and a sequence, opaque to clients. "Too old" answers a token
from before what the log keeps, or from another epoch: a rebuilt index
starts a new one. So the journal comes in phase 1, with the log, not
in phase 2.

As built (phase 1, step 4): a record says what one folder now is, its
facts, its state and each change to its entries (added, removed or
changed, a removal with what the entry was), and a refresh that
completes adds when it proved the names. Each record has a checksum: a
record a crash cut short ends the log, and is cut off before the next
append. The manifest says how far into the log the chunks go, so a load
replays the rest. A load claims "as of" when the names were last
proven, which the manifest and the log record, never the time of the
save, which may be later. An epoch ends where the journal cannot go on
whole: a rebuild, a log that could not be written (the next save
starts another), a log grown past 16 MB at a save, and a purge.

**A purge leaves nothing on disk.** A folder made private, or registered
by a client, is let go of in the index at once; its names that no other
entry holds are written empty (each keeps its number, so no chunk
changes); the log that held them ends its epoch and is removed; and the
index is saved, so the chunks that held them are removed too.

The index is a cache: a damaged file, or one of another version or
layout, is rebuilt. Its header records every array's width, so two
layouts never pass for one: the port keeps kinds in 4 bits on macOS and
5 on Linux under one version, on the promise that the two files never
meet. A save that fails keeps the index in memory, and its "as
of" stays at the last save that succeeded: fsearch reloaded the older
file and lost every change since, until 0.2.1. On disk, the last
manifest, the files it names and the log stay, so a start after it
replays to where the log ends.

`skwird-bench walk` estimates its size, every field at its full width:
33 bytes per entry, 16 per distinct name plus its bytes, and 40 per
folder. For the home phase 0 measured, 341 MB for 5.8 million entries.
Packed, it is smaller: on a developer's work folder of 2.9 million
entries, 29.3 bytes an entry, 63% of the estimate for that tree
(`docs/PLAN.md`, phase 1, step 2). The measured home's own size waits
for step 6. That home's distinct names were 44% of its entries, so a
query scores fewer than half as many names as entries, and storing
each name once cost no more than storing every entry's (124 MB either
way).

### 9. Every system

Linux, macOS and Windows:
- Code for one system sits behind `cfg`, with the others' answer
  beside it: their own code, or an error naming the system.
- Windows is tried under Wine as far as Wine goes.
- macOS is compiled only, until someone with a Mac helps.

### 10. Idle is idle, and nothing leaves the machine

There is no polling loop: the engine sleeps until the system notifies
it. It never uses the network.

**Budgets.** Set on 2026-11-21 from phase 1's measurement of a home of
6.9 million entries in 867,000 folders: btrfs on a solid-state disk, 20
threads, 32 GB. They are per million entries, so that they scale with
the tree. A slower disk lengthens what is the disk's (a cold crawl, a
cold check), not what is the engine's. The engine's rows are the third
measurement's, after phase 1's step 8 (at `7f98a7d`); the build's row is
the first measurement's (at `8850b44`).

| What | Budget, per million entries | Measured, per million (whole home) | |
|---|---|---|---|
| Processor time while idle | none measurable (the kernel counts in 10 ms ticks) | 0.00 s in 30 s | met |
| First answer after a start, the index read back | 0.15 s | 0.09 s (0.63 s), as of its last save | met |
| A sync, warm | 0.15 s | 0.06 s (0.43 s) | met |
| The build, from a crawl | 1 s | 0.61 s (4.2 s) | met |
| Memory at rest, the index read back | 40 MB | 26 MB (178 MB) | met |
| Memory after a first build, once saved | as at rest | 21 MB (144 MB) | met |
| Memory at the peak of a first build | 150 MB | 87 MB (598 MB) | met |
| Memory idle, after a sync | as at rest | 21 MB (144 MB) | met |

Memory is what the kernel counts resident (`VmRSS`, and `VmHWM` for the
peak). The index's mapped files count once read, and are the page
cache's to drop. A first start, with no index yet, answers once the
whole tree is crawled and built. With the caches warm, that took 5.6 s
at the second measurement and 8.6 s at the third; at most about 20 s
cold, by the first measurement's cold crawl and build.

**What memory was over budget was the allocator's:** small allocations,
freed and kept for the process. Measured first, the engine stayed at
1,873 MB after a first build (2,268 MB at the peak), and at 384 MB idle
after a sync, against 178 MB read back. Phase 1's step 7 packs each
folder as the crawl reads it, into a few large buffers. Step 8 keeps the
check's list of every folder in one buffer, where each path had an
allocation of its own. It also fixes glibc's threshold for giving a
large allocation a mapping of its own (`mallopt`, 1 MiB): otherwise the
threshold rises each time such a mapping is freed, and the next large
buffers come from an arena that keeps them. What is free is given back
after a first build and after each refresh (`malloc_trim`).

**Budgets for live folders** (phase 2), set on 2026-11-22 from the live
step of `tools/measure.sh` on the user's home: the engine watching as
much of it as its share allows (131,072 folders, the most a user's
engine holds), dot folders first, caches and profiles among them.
Latency, install and memory come from two runs on a quiet machine (at
`ba3ed1e` and `2e7ad76`); processor time from the last (at `7d2232f`),
at a load of 20 to 35, since the duty cycle (8) counts the engine's
own time, whatever the load.

| What | Budget | Measured | |
|---|---|---|---|
| A change in a watched folder to its answer, no sync | 0.25 s at the median, 1 s at most | 136 to 158 ms, 161 to 179 ms at most | met |
| A sync of a live folder | 0.1 s | 54 ms, loaded | met |
| Watching, from a registration | 5 s per 100,000 folders | 3.4 to 5.6 s for 131,072 | met |
| Processor time watching, no client asking | 2% of a core, whatever the folders do; none measurable where nothing changes | 0.53 s in 30 s (1.8%); 0.00 s on a synthetic tree at rest | met |
| Memory watching, idle | at rest and 1 KB a watch (312 MB here) | 108 to 242 MB with 131,072 watches; at rest, 181 MB | met |

The kernel keeps about 1 KB of its own memory for each watch, not
counted in the engine's: 131 MB for the share. Just after an install,
the engine held up to 353 MB: the index's pages the install read, the
page cache's to drop. A folder whose reads would take more than its
share of a core lags by more than a second, live still; a sync applies
it at once.

**Budgets for the system service** (phase 3), set on 2026-11-22 from
the system step of `tools/measure.sh` on the user's machine, at
`50c540d`: seven roots on two filesystems (a btrfs, marked once through
its top level, and another), 6.95 million entries in 830,581 folders, at
a load of 4 to 5. Memory is per million entries, as phase 1's.

| What | Budget | Measured | |
|---|---|---|---|
| A change to its answer, no sync | 0.25 s at the median, 1 s at most | 231 ms, 354 ms at most | met |
| A sync of a live folder | 0.1 s | 0.09 s | met |
| First live answer, read back and every root checked | 0.3 s per million entries | 1.35 s (0.19 s per million) | met |
| A query answered to a user, filtered for them, against root's | 1.25 times root's | 0.63 s against 0.54 s for 100,000 results (1.17) | met |
| Memory at rest | as a user's engine's, 26 MB per million entries | 168 to 187 MB (24 to 27 MB per million) | met |
| Memory at the peak of a first build | 150 MB per million entries, as phase 1's | 584 MB (84 MB per million) | met |
| Processor time, no client asking | 2% of a core | 0.85 s in 30 s (2.8%) | not met |

Not met: the duty cycle (8) holds what applying what is heard costs,
not what finding each event's folder does (a handle opened, its path
read and checked, for each folder heard in each batch), nor the saves.
The machine's own activity was heard meanwhile: 2,875 events while it
built, then 100 to 160 in each 10 s, a cache folder of 58,919 entries
read again up to 20 times in one. Next:
the whole of what the writer does for what is heard held to the share.

Not set: the first build. The run built every root in 28.9 s, the home
in 27.6 s, three times phase 1's warm build of it; the step ran alone,
nothing warming the home before, and a crawl stats every entry, so it
was near cold. The flag of folders with an ACL (one `fgetxattr` each)
cost 2.5 to 8% on a warm synthetic tree of 100,000 folders, not that.
A warm run sets it. The index on disk was 294 MB.

## Protocol (a sketch; experimental until declared stable)

- **Transport.** A Unix socket on Linux and macOS: `/run/skwird/socket`
  for the system service, `$XDG_RUNTIME_DIR/skwird/socket` for a user's
  engine. A named pipe on Windows: `\\.\pipe\skwird`, and one per user
  named by its SID. A socket path longer than the system allows (108
  bytes on Linux, 104 on macOS) is an error that says so, not the crash
  it was in fsearch. The user's folder is the user's alone (mode 0700),
  checked so before the socket is bound, as skwir keeps its own runtime
  folder. A service manager may make the socket and pass it
  (systemd's `LISTEN_FDS`, phase 2): the engine then answers on it, and
  neither binds nor removes one of its own.
- **Framing.** JSON lines at first, for readability; each message has
  an id.
- **Messages:**
  - `hello`: the protocol versions spoken, the scope, a summary of
    coverage;
  - `coverage {root}`;
  - `sync {root}`: the barrier, or the check where nothing watches the
    root (2);
  - `query {root, names, kinds, sizes, times, limit, rank}`: streamed
    batches of results, then `done` with the coverage that held;
  - `cancel {id}`;
  - `changes {root, since}`: what changed since a token, with each
    entry's identity and kind, then the token to resume from; or "too
    old" (8). A journal for clients that keep state of their own. Since
    nothing (`since` empty): from now, no event and the token to go on
    from, which a client takes before it walks, first or after "too
    old", so that nothing is lost between (found missing on 2026-11-22:
    no client could start the journal). At the system service, an event
    is given only to who may list the folder holding what it names;
  - `exclude {add, remove}`: folders the client keeps out of the index
    (5). A client removes only what it added.
  - `watch {add, remove}` (phase 2): folders the client wants live,
    kept as its exclusions are (open question 2). An event in one is
    applied by reading its folder again: every name its listing holds,
    stat-ing the entries its events named and what is new (DESIGN 8).
- **Versions.** The protocol is experimental: a version may change
  until it is declared stable. Once stable, skwird keeps speaking the
  older versions. A client and an engine with no version in common do
  without each other, and the client says so (1).
- **The crates clients take** (named on 2026-11-21):
  - `skwird-protocol`: the messages, which the engine speaks too;
  - `skwird-client`: a client (finding an engine, starting one, asking
    it) and a fake engine for clients' tests, built on
    `skwird-protocol`.

  Both depend on serde and serde_json alone, and neither is published
  on crates.io (the user's choice, 2026-11-11). A client copies them
  into its own tree: skwir takes no git dependency (its ADR-050), and
  keeps them as `vendor/skwird-protocol` and `vendor/skwird-client`, as
  it keeps gpui and syntect. skwird's `tools/vendor-client` writes
  those copies as `cargo package` writes a crate, their manifests
  standing alone, outside skwird's workspace, and names the commit they
  came from. A client may instead write its own, from this section and
  the transcripts the tests keep.

**As built** (phase 1, step 5):
- The messages are `skwird-protocol`'s types, and
  `crates/protocol/transcripts/` holds a session of every one, which
  the crate's test holds to them.
- A client that finds no engine starts `skwird serve` in a process group
  of its own, its output to `$XDG_STATE_HOME/skwird/engine.log` (counts
  and times, never a name or a path), and waits for its socket up to
  five seconds. `SKWIRD_IDLE` sets how long such an engine stays once
  its last client has gone (60 seconds).
- The user's exclusions are `$XDG_CONFIG_HOME/skwird/exclude`, one
  path a line; what clients register is kept in
  `$XDG_STATE_HOME/skwird/registered`; each root's index is in
  `$XDG_CACHE_HOME/skwird/index-<the hash of its path>`.
- A `sync` refreshes the whole root that holds the path asked: it is
  cheap (0.65 s warm for the home phase 1 measured, 82 ms of it the
  check of every folder), and a proof is
  kept for the whole index, not a subtree. Syncs asked while one runs
  are answered by the next refresh.
- A query is answered from the snapshot published when it is asked;
  `cancel` stops the batches still to be sent, and its `done` says so.

## Open questions

1. **Cookies:** answered for inotify on 2026-11-21 (phase 2, step 4). A
   user's engine has one inotify instance, so one queue: its cookies go in
   a folder of its own in its runtime folder, whatever filesystem the
   folders it watches are on. Answered for fanotify on 2026-11-22 (phase
   3, step 3): a group has one queue for all its marks, so a cookie made
   on any filesystem marked comes after every event queued before it, on
   any other; a cookie on ext4 came after a change made just before on
   a btrfs. Open for the change journal. FSEvents flushes
   instead (`FSEventStreamFlushSync`).
2. **Engines and watches:**
   - how a user's engine and the system service share work;
   - which folders a user's engine watches on Linux: answered on
     2026-11-21, for phase 2. Those its clients register (`watch`), kept
     per client across restarts as exclusions are, within a quarter of
     the user's inotify watches: editors, file managers and build tools
     share the rest. The check covers every other folder, and a folder
     past the share is checked, not live (a watch limit reached). A
     watch per folder for a whole home does not fit: phase 1 counted
     866,584 folders against a limit of 524,288.
3. **A user's exclusions:** answered on 2026-11-11 (5). The user's
   client registers folders over the socket, with the user's
   credentials, and the service keeps them per user, each applying only
   to folders that user owns.
4. **Content search:** answered on 2026-10-26 by skwir's ADR-089, and
   taken in on 2026-11-21: never in skwird. Derived facts and their
   indexes are per user, built by the user's own client, never by a
   service that reads other users' files. skwir measured a trigram
   index at 11% of the text, 5 to 175 times faster than scanning, and
   builds it in its own run.
5. **Names:** protocol 0 matches as skwir does (7). Open: Unicode case
   folding beyond `to_lowercase`, normalization, and fuzzy scoring.
   fsearch keeps names as the disk has them and searches each query in
   both its composed and decomposed forms (NFC and NFD), a way to meet
   normalization without changing names. A change here changes skwir's
   matching with it, or answers would hold less than skwir matches.
6. **fanotify on btrfs subvolumes:** answered by phase 0, as far as a
   trial goes (decision 3). A mark through the top-level subvolume,
   mounted read-only and detached, heard the changes made in three
   subvolumes on Linux 6.18.55, and their folder handles opened. Phase
   3's tests decide whether it holds live.
7. **Windows:** whether NTFS folder times can serve as the check; ReFS;
   how fast the file table reads.
8. **Removable volumes:** indexed unasked, or asked first, perhaps once
   per volume by its UUID, as skwir's harvest asks. Roots under the
   home reach none, so phase 1 does not meet them. Network volumes are
   answered (2): never crawled.
9. **The name:** whether `skwird` is free in the distributions. No
   crate is published on crates.io (the user's choice, 2026-11-11).
10. **Snapshots** (found on 2026-11-22, phase 3, step 4): a btrfs
    snapshot is a subvolume, not a mount, so a root holding snapshots
    (snapper's `/.snapshots`, timeshift's, btrbk's) would index each
    whole, a copy of the system each. Answered on 2026-11-22, the
    user's choice: a snapshot below a root, a read-only subvolume (its
    top folder inode 256, btrfs saying read-only), is left out, its name
    kept, not covered as excluded (no reason of its own: a client of the
    crates vendored now would not read one); a root named as a snapshot
    is read. A root test proves both, beside a writable subvolume read.
