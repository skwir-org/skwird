# skwird's design

Status: design, 2026-10-10. Only `skwird-bench` (phase 0, `docs/PLAN.md`)
is built. Each decision says why, so that it can be revisited knowingly.

## What skwird is

An index of the names of every file under the folders it covers, for
Linux, macOS and Windows. It is kept current by the system's change
notifications and answered over a local socket in milliseconds. Every
answer says how far it can be trusted.

It is a general tool: a command line, and a protocol any program can
speak (file managers, launchers, editors). bibli, a file manager, is
its first client, and the reason several decisions below exist.

It borrows from two MIT-licensed projects, with credit where code is
taken:

- [fsearch](https://github.com/noahdunnagan/fsearch) (macOS):
  - the index layout and name interning;
  - the overlay of changes over a base;
  - FSEvents replay.
- [fsearch-crossplatform](https://github.com/threehymns/fsearch-crossplatform):
  - a Linux walker with `getdents64`;
  - inotify watching per folder;
  - catching up by folder times after a restart.

It does none of these:

- **Search inside files.** Not for now (open question 4).
- **Types by extension.** Clients type files; bibli uses the desktop's
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
by a lock. A user's engine that finds the system service covering its
roots live (2) leaves them to it.

The engine's crates can be embedded by other programs. Clients are
supported through the protocol only, for four reasons:

- **One code path.** There is one protocol to test, not a library mode
  and a socket mode that must behave alike.
- **One set of watches per scope.** In fsearch-crossplatform, an
  application linking the engine follows the daemon's index but
  installs its own watches: one inotify watch per folder per process.
  Its README counts 465k watches for one developer's home, against a
  limit of 524k, so two processes do not fit.
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
- **Not covered**, and why: excluded, permission denied, a network
  share, a watch limit reached, a mount below the root.

The kinds of fact are:
- **names** (and kinds: file, folder, link, other);
- **attributes** (identity, size, times);
- **content**, later.

Coverage is never overstated. An overflow, a failed watch, a mount
appearing or a folder denied lowers it at once, for the subtree
concerned, until that subtree is read again.

- **The barrier** makes "live" mean up to now. The client asks for a
  sync. skwird creates a cookie file in a folder of its own that shares
  the notification queue, and answers once it hears it. Where the
  system has a flush (`FSEventStreamFlushSync`), that is used instead.
  Watchman and git's fsmonitor daemon work the same way. A rename the
  client just made is then in the answer.
- **The check** proves names current without any watch. A folder's
  change time moves whenever its entries change. So comparing each
  folder's change time with the one stored, for equality, finds every
  folder to read again. It is not a comparison against a timestamp with
  a margin, as fsearch-crossplatform does with modification times:
  `tar` and `rsync -a` set those back after filling a folder, and
  cannot set a change time. This holds only where change times tell:
  local filesystems, not network shares or FUSE. It proves names, not
  attributes: a file's size changes no folder's time.
- **Answers are exact for covered facts under covered roots.** Clients
  still check what they show (a stat), so a stale result costs one
  check. The guarantee is about results missing.
- **The index reaches what a walk reaches.** Hidden names are indexed
  (clients filter them), links are never followed, and each mount below
  a root is reported, covered or not.

### 3. Privileges are detected, and become coverage

There is no "root mode". Whatever starts skwird grants it capabilities:
systemd's `AmbientCapabilities`, launchd, a Windows service account.
skwird uses what it was given and reports the coverage that results.

| | As a user | With privileges |
|---|---|---|
| Linux | `getdents64` walk. inotify needs one watch per folder, about 1 KB of kernel memory each, within `max_user_watches`. No history across restarts: the check (2) catches up | fanotify on whole filesystems (`FAN_MARK_FILESYSTEM`, `FAN_REPORT_DFID_NAME`: Linux 5.9 or later, `CAP_SYS_ADMIN`). No watch per folder, every user's files. Still no history across a reboot |
| macOS | `getattrlistbulk` walk. FSEvents keeps a history on disk, so a restart replays what changed | Root adds little; Full Disk Access decides what can be read |
| Windows | `FindFirstFileEx` walk. `ReadDirectoryChangesW` watches a whole tree with one handle. No history | The volume's file table to crawl and its change journal (USN) to catch up after a restart, as Everything does. Needs administrator rights and NTFS |

As a user on Linux, the check is the base mechanism, and inotify watches
are a bounded extra for the folders that change most (open question 2).

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
- a `.private` file in a folder (bibli's convention);
- default exclusions (`~/.ssh`, `~/.gnupg`, keyrings);
- a user's own exclusions.

A client cannot widen what is indexed. How a system service learns a
user's exclusions is open (question 3).

### 6. Answers carry identity

Each entry comes with its device and inode, and its folder's. That lets
a client:
- tell a file from its names (hard links);
- follow a rename;
- keep what belongs to the file by the file (bibli's ADR-049).

The cost is 8 bytes per entry; the device is one per filesystem's index.

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

From fsearch:
- one file per filesystem, mapped into memory;
- entries in folder blocks, depth first, so a folder's whole subtree is
  one range;
- each distinct name stored once with a mask of its letters, so a query
  scores names, not entries;
- a base, and an overlay of changes with tombstones.

skwird adds:
- each entry's inode;
- each folder's change time, owner, group, mode and ACL flag;
- each filesystem's identity (its UUID where the system gives one), so
  a removable drive's index survives being unplugged;
- changes written as segments merged in the background, never the whole
  index rewritten. fsearch-crossplatform rewrites everything when its
  overlay grows: about 280 MB for 7.7M entries.

The index is a cache: a wrong version or a damaged file is rebuilt.

`skwird-bench walk` estimates its size: 33 bytes per entry, 16 per
distinct name plus its bytes, and 40 per folder.

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
  named by its SID.
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
   each client. bibli measured one: 11% of the text, 5 to 175 times
   faster than scanning.
5. **Names:** their exact meaning (Unicode case folding and
   normalization), and fuzzy scoring.
6. **fanotify on btrfs subvolumes:** `skwird-bench fanotify` asks.
7. **Windows:** whether NTFS folder times can serve as the check; ReFS;
   how fast the file table reads.
8. **Removable and network volumes:** indexed unasked, or asked first.
9. **The name:** whether `skwird` is free on crates.io and in the
   distributions.
