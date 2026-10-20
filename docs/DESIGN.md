# skwird's design

Status: design, 2026-10-10, revised on 2026-10-11 by phase 0's numbers
and on 2026-10-20 by what fsearch and its Linux port changed since.
Only `skwird-bench` (phase 0, `docs/PLAN.md`) is built. Each decision
says why, so that it can be revisited knowingly.

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
  - the overlay of changes over a base;
  - FSEvents replay, of events the system dropped as well.
- [fsearch-crossplatform](https://github.com/threehymns/fsearch-crossplatform):
  - a Linux walker with `getdents64`;
  - inotify watching per folder.

The bugs both fixed are lessons too, recorded below where they apply.
Both were last read on 2026-10-20, at fsearch 0.2.1 (`88e63fd`) and
fsearch-crossplatform `8857eec`.

It does none of these:

- **Search inside files.** Not for now (open question 4).
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
4. otherwise it does without (a client must work without skwird).

There is one engine per scope: one for the system, one per user, held
by a lock. A second engine blocks on the lock and takes over when the
first exits, even during its first crawl; it does not poll the lock, as
fsearch's followers do every 10 seconds (10). A user's engine that
finds the system service covering its roots live (2) leaves them to it.

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
- **Not covered**, and why: excluded, permission denied, a read that
  failed, a network share, a watch limit reached, a mount below the
  root.

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
- **A dropped event ends "live"** until the replay or the reread is
  done. Where the system keeps a history (FSEvents, the change journal),
  the position kept to resume from is never past an event not yet
  applied.
- **The barrier** makes "live" mean up to now. The client asks for a
  sync. skwird creates a cookie file in a folder of its own that shares
  the notification queue, and answers once it hears it. Where the
  system has a flush (`FSEventStreamFlushSync`), that is used instead.
  Watchman and git's fsmonitor daemon work the same way. A rename the
  client just made is then in the answer.
- **The check** proves names current without any watch. A folder's
  change time moves whenever its entries change. So comparing each
  folder's change time with the one stored, for equality, finds every
  folder to read again. Three rules keep this sound:
  - The time stored is read before the folder's entries, so a change
    during the read leaves a time that differs.
  - Change times can be coarse: a second change within one tick of the
    clock can leave the time as it was, so a folder read within a tick
    of its last change is read again at the next check, as git does
    with its "racily clean" entries.
  - The folder's device and inode are compared too. The check goes by
    path, which phase 0 found fastest, and a path follows a link in any
    of its folders (`O_NOFOLLOW` guards only the last name). A folder
    replaced, or reached through a link put in place of a folder above
    it, is then another folder, and is read again. A folder is read
    through its parent's handle, or by a path that may hold no link
    (`openat2` with `RESOLVE_NO_SYMLINKS` on Linux, `O_NOFOLLOW_ANY` on
    macOS), so another folder's names are never indexed under its path.
    A path longer than the system allows is checked through its
    parent's handle as well.

  It is not a comparison against a timestamp with a margin, as fsearch
  does with modification times (every folder changed since 120 seconds
  before its last sync), and its port with it: `tar` and `rsync -a` set
  those back after filling a folder, and cannot set a change time. This
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
  (clients filter them), links are never followed, and each mount below
  a root is reported, covered or not. A mount is told by its mount id
  (`statx`, Linux 5.8), not by its device: each btrfs subvolume has a
  device of its own and is no mount, and a bind mount keeps its
  filesystem's device. fsearch-crossplatform tells mounts by device, so
  it skips subvolumes and walks through bind mounts. The crawl, the
  check and a reread share one test, which holds at the mount's own
  folder as well: fsearch once crawled a whole volume when a change was
  reported on its mount point.

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

### 4. A system service answers only what the asker could list

- **Who asks** comes from the transport: `SO_PEERCRED` on Linux,
  `getpeereid` on macOS, impersonating the named pipe's client on
  Windows.
- **A result is given only if the asker could list its folder and enter
  every folder above it.**
  - The index keeps each folder's owner, group and mode, and whether it
    has an ACL.
  - Plain modes are evaluated against the asker's user and groups.
  - A folder with an ACL, or under a security module's rules, is tried
    as the asker. On Linux that is a thread taking the asker's
    filesystem ids (`setfsuid` and `setfsgid` are per thread). On
    Windows it is a check while impersonating.
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

### 5. Private folders are never indexed

It is not enough to leave them out of answers: they are never read into
the index. skwird honours what marks them itself:
- a `.private` file in a folder (skwir's convention);
- default exclusions (`~/.ssh`, `~/.gnupg`, keyrings);
- a user's own exclusions.

A client cannot widen what is indexed. How a system service learns a
user's exclusions is open (question 3).

An exclusion, like a root, is resolved once to the folder it names, and
compared as that folder (its identity, or its range in the index),
never as the spelling of a path. In fsearch, an exclusion written with a
trailing slash excluded nothing, an empty one excluded everything, and
`/h/m` was taken to hold `/h/me`. In skwird the first would index a
private folder.

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
  index;
- a base, and an overlay of changes with tombstones.

skwird adds:
- one file per filesystem, where fsearch keeps one for all and does not
  cross a mount;
- each entry's inode;
- each folder's change time, owner, group, mode and ACL flag;
- each filesystem's identity (its UUID where the system gives one), so
  a removable drive's index survives being unplugged;
- the overlay keyed by folder and name, not by path, so removing a
  folder removes everything under it. fsearch's tables keyed by path
  kept a removed folder's new files searchable, until 0.2.1;
- changes written as segments merged in the background, never the whole
  index rewritten. fsearch rewrites everything after 50,000 changes or
  12 hours, about 280 MB by its own estimate, and its port does the
  same.

The index is a cache: a damaged file, or one of another version or
layout, is rebuilt. Its header records every array's width, so two
layouts never pass for one: the port keeps kinds in 4 bits on macOS and
5 on Linux under one version, on the promise that the two files never
meet. A save that fails keeps the index in memory, and its "as
of" stays at the last save that succeeded: fsearch reloaded the older
file and lost every change since, until 0.2.1.

`skwird-bench walk` estimates its size, every field at its full width:
33 bytes per entry, 16 per distinct name plus its bytes, and 40 per
folder. For the home phase 0 measured, 341 MB for 5.8 million entries.
Packing as fsearch does should shrink it; phase 1 measures it. Its
distinct names were 44% of its entries, so a query scores fewer than
half as many names as entries, and storing each name once cost no more
than storing every entry's (124 MB either way).

### 9. Every system

Linux, macOS and Windows:
- Code for one system sits behind `cfg`, with the others' answer
  beside it: their own code, or an error naming the system.
- Windows is tried under Wine as far as Wine goes.
- macOS is compiled only, until someone with a Mac helps.

### 10. Idle is idle, and nothing leaves the machine

There is no polling loop: the engine sleeps until the system notifies
it. It never uses the network. Budgets (memory per million entries,
idle CPU, time to the first answer after a start) are set once phase 1
measures them.

## Protocol (a sketch; experimental until declared stable)

- **Transport.** A Unix socket on Linux and macOS: `/run/skwird/socket`
  for the system service, `$XDG_RUNTIME_DIR/skwird/socket` for a user's
  engine. A named pipe on Windows: `\\.\pipe\skwird`, and one per user
  named by its SID. A socket path longer than the system allows (108
  bytes on Linux, 104 on macOS) is an error that says so, not the crash
  it was in fsearch.
- **Framing.** JSON lines at first, for readability; each message has
  an id.
- **Messages:**
  - `hello`: the protocol versions spoken, the scope, a summary of
    coverage;
  - `coverage {root}`;
  - `sync {root}`: the barrier (2);
  - `query {root, names, kinds, sizes, times, limit, rank}`: streamed
    batches of results, then `done` with the coverage that held;
  - `cancel {id}`;
  - `changes {root, since}`: what changed since a token, or "too old"
    (a journal for clients that keep state of their own).
- **Versions.** The protocol is experimental: a version may change
  until it is declared stable. Once stable, skwird keeps speaking the
  older versions.

## Open questions

1. **Cookies:** where they go on each filesystem, one per notification
   queue (fanotify has one queue for all its marks).
2. **Engines and watches:**
   - how a user's engine and the system service share work;
   - which folders a user's engine watches on Linux.
3. **A user's exclusions:** how a system service learns them. Perhaps
   the user's client registers them over the socket, with the user's
   credentials, and the service keeps them per user.
4. **Content search:** whether a trigram index belongs in skwird or in
   each client. skwir measured one: 11% of the text, 5 to 175 times
   faster than scanning.
5. **Names:** their exact meaning (Unicode case folding and
   normalization), and fuzzy scoring. fsearch keeps names as the disk
   has them and searches each query in both its composed and decomposed
   forms (NFC and NFD), a way to meet normalization without changing
   names.
6. **fanotify on btrfs subvolumes:** answered by phase 0, as far as a
   trial goes (decision 3). A mark through the top-level subvolume,
   mounted read-only and detached, heard the changes made in three
   subvolumes on Linux 6.18.55, and their folder handles opened. Phase
   3's tests decide whether it holds live.
7. **Windows:** whether NTFS folder times can serve as the check; ReFS;
   how fast the file table reads.
8. **Removable and network volumes:** indexed unasked, or asked first.
9. **The name:** whether `skwird` is free on crates.io and in the
   distributions.
