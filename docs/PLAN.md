# skwird's plan

Each phase is useful alone, and ends with what was measured and what is
left, honestly. The design is `docs/DESIGN.md`; a phase that changes a
decision changes it there.

## Phase 0: measure (now)

What decides how much each system needs a service:

- **The walk.** How long reading every folder of a real home takes, and
  how large its index would be (`skwird-bench walk`, and with `--stat`
  for sizes and times).
- **The check.** How long proving names current takes where nothing
  watched: every folder's change time, warm and cold, on 1, 8 and all
  threads (`skwird-bench check`, `--cold`).
- **fanotify.** Whether this kernel lets a system service watch a whole
  btrfs filesystem, and from which subvolumes the events come
  (`skwird-bench fanotify`, as root, with probes: folders where it makes,
  renames and removes a file of its own and says whether it heard them).

`tools/measure.sh` runs all three on a folder, cold and warm, and keeps
the output in one file.

Measured on 2026-10-10, on skwir's synthetic `deep1m` tree (btrfs,
warm, 20 threads, an idle machine):
- **Walk:** 1,011,113 entries in 11,111 folders. 243 ms reading names,
  846 ms stat-ing every entry.
- **Check:** 1.1 to 1.9 ms on 8 to 20 threads, 9 to 13 ms on one.
  Stat-ing each folder by its path was faster than through its parent
  held open, beyond one thread.
- **fanotify:** a mark of the whole filesystem needs root, as
  documented.

Measured on 2026-10-11, on a developer's home, by `tools/measure.sh`
(`skwird-bench` at 488797c; btrfs on a solid-state disk, Linux 6.18,
20 threads, 32 GB, an idle machine):
- **The tree:** 5,797,916 entries: 652,336 folders, 5,080,491 files,
  64,119 links and 970 others. No mount below it, no folder unreadable.
  2,566,287 distinct names, 44% of the entries.
- **Walk:** reading names, 12.7 s cold and 966 ms warm. Stat-ing every
  entry, 10.0 s cold and 2.70 s warm. One run each: stat-ing every
  entry did not make the cold walk slower.
- **Check:** by path, 1.86 s cold and 52 to 55 ms warm on 20 threads,
  3.22 s and 92 ms on 8, 29.5 s and 600 ms on one. Through parents held
  open, 12.8 s cold on 20 threads, seven times slower than by path: the
  check goes by path. Each pass found 6 or 7 folders changed since its
  walk, by what the machine was doing meanwhile.
- **Cold:** every cold pass was 4 to 76 times slower than its warm one,
  so the caches were dropped.
- **Index:** estimated at 341 MB (entries 191 MB, names 124 MB, folders
  26 MB), 59 bytes per entry.
- **inotify:** a watch per folder would take 652,336 watches. This
  user's limit is 524,288.
- **fanotify,** as root: the mark of the whole filesystem was refused
  with `EXDEV`. The home is a btrfs subvolume, whose filesystem id is
  not its superblock's. The probes did not run.

Tried again on 2026-10-11, as root (`skwird-bench` at 2fd2c66, Linux
6.18.55, the same machine):
- **fanotify:** the mark through the home was refused with `EXDEV`, as
  before. The mark through the top-level subvolume, mounted read-only
  and detached, was accepted.
- **Probes:** three, each in a subvolume with a filesystem id other
  than the mark's. All nine of their changes (a file made, renamed and
  removed in each) were heard, with the mark's filesystem id. Each
  event's folder handle opened, through the detached mount, to its own
  folder, by device and inode.
- **Queue:** 17 events in two seconds, no overflow. The count includes
  what the machine did meanwhile. It measures no sustained load, and no
  recovery from an overflow.

On 2026-10-20 the walk learned to tell a mount by its mount id, not its
device (DESIGN 2). The home measured had no folder of another device
below it, so its numbers stand.

**Done when:** a real home (and a cold cache) is measured, fanotify is
tried as root, and the design is revised where the numbers say so.
Done on 2026-10-11. fanotify's way through btrfs is tried, not proven
live: that is phase 3's (DESIGN 3).

## Phase 1: Linux, a user's engine, started by its first client

- **Crates:**
  - `skwird-index`: the format, and queries on it, with fsearch 0.2's
    packed arrays and zones (DESIGN 8);
  - `skwird-crawl`: the Linux walker;
  - `skwird-proto`: the messages, and a client;
  - `skwird`: the program, which serves and is the command line.
- **The check** as the way to coverage (DESIGN 2), reported in every
  answer.
- **Protocol 0**, experimental.
- **Tests:** coverage is never overstated. Fixtures cover:
  - a change made while the engine was down;
  - a folder whose times `tar` set back;
  - a folder changed again within a tick of being read;
  - a mount below the root, a bind mount of the same filesystem (made
    in a user namespace), and a reread that starts at a mount;
  - a btrfs subvolume below the root: walked, with a device of its own;
  - a folder denied, then made readable: not covered, then read at the
    next check;
  - a listing that fails part-way: not covered, never empty;
  - a name added during the crawl;
  - a folder, or a folder above it, replaced by a link: another
    folder's names never appear under its path;
  - a folder that gains entries, then is removed or replaced by a file:
    nothing under it remains;
  - a tree deeper than 256 folders, its paths longer than the system
    allows;
  - an exclusion written with a trailing slash, a doubled slash, or
    through a link: it excludes its folder, and only it.
- **Tests of the engine:**
  - after every batch of changes, each entry's parent is a folder in
    the index;
  - queries made at once answer as queries made one by one;
  - the first engine exits during its first crawl, and the second takes
    over;
  - a save that fails keeps the index, and its "as of";
  - a socket path too long is an error that says so.

**Done when:** a name query of a home tree gives what a walk would,
after a restart as well; and the memory, the time to the first answer,
idle CPU and the index's size (against `skwird-bench`'s estimate) are
measured, and the first three become budgets.

## Phase 2: Linux, a user service

- A systemd user unit.
- inotify for a bounded set of folders, read through rustix. A wrapper
  can pass an overflow on as an ordinary event: `notify` does, and
  fsearch-crossplatform misses it.
- Each folder watched before it is read or checked (DESIGN 2).
- The sync barrier (cookies).
- The `changes` journal.
- Tests, without root: an overflow (a queue left unread while more
  files are made than `max_queued_events`) lowers coverage at once; a
  change made while the watches are being installed is not missed.

## Phase 3: Linux, the system service

- fanotify on whole filesystems. On btrfs, use a detached read-only
  top-level mount when marking through a subvolume is refused (DESIGN 3).
- Coverage tests for queue overflow, mount and subvolume changes, and
  handle resolution failures; phase 0 establishes feasibility only.
- Answers filtered per asker (DESIGN 4).
- The sandboxed unit.
- Tests that need root run in a virtual machine, never on the
  developer's own system.

## Phase 4: Windows

- A user's engine first: the walk and `ReadDirectoryChangesW`.
- Then the service: the file table and the change journal.
- Tried under Wine as far as Wine answers.

## Phase 5: macOS

- `getattrlistbulk` and FSEvents, from fsearch:
  - events the system dropped, replayed from its log;
  - the position kept never past a gap;
  - the callback at user-initiated priority, which cut fsearch's drops
    under load;
  - the stream torn down in fsearch's order (stop, invalidate while
    still scheduled, release).
- Folders macOS asks consent for (Desktop, Documents, Music and others):
  not covered, for want of consent, and tried again at each start, as
  fsearch does, since no event follows a consent given.
- A LaunchAgent, and a LaunchDaemon for the system scope.

## Later

- Content search, if decided (DESIGN, open question 4).
- Packages: deb, rpm, AUR, Nix, Homebrew, winget.
- CI once there are crates to check on three systems. The development
  shell (`flake.nix`) already carries the Windows and macOS targets.
