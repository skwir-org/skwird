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
  (`skwird-bench fanotify`, as root).

Measured so far, 2026-10-10, on bibli's synthetic `deep1m` tree (btrfs,
warm, 20 threads, an idle machine):
- **Walk:** 1,011,113 entries in 11,111 folders. 243 ms reading names,
  846 ms stat-ing every entry.
- **Check:** 1.1 to 1.9 ms on 8 to 20 threads, 9 to 13 ms on one.
  Stat-ing each folder by its path was faster than through its parent
  held open, beyond one thread.
- **fanotify:** a mark of the whole filesystem needs root, as
  documented; with root, not tried yet.

**Done when:** a real home (and a cold cache) is measured, fanotify is
tried as root, and the design is revised where the numbers say so.

## Phase 1: Linux, a user's engine, started by its first client

- **Crates:**
  - `skwird-index`: the format, and queries on it;
  - `skwird-crawl`: the Linux walker;
  - `skwird-proto`: the messages, and a client;
  - `skwird`: the program, which serves and is the command line.
- **The check** as the way to coverage (DESIGN 2), reported in every
  answer.
- **Protocol 0**, experimental.
- **Tests:** coverage is never overstated. Fixtures cover:
  - a change made while the engine was down;
  - a folder whose times `tar` set back;
  - a mount below the root;
  - a folder denied;
  - a name added during the crawl.

**Done when:** a name query of a home tree gives what a walk would,
after a restart as well; and the memory, the time to the first answer
and idle CPU are measured and become budgets.

## Phase 2: Linux, a user service

- A systemd user unit.
- inotify for a bounded set of folders.
- The sync barrier (cookies).
- The `changes` journal.

## Phase 3: Linux, the system service

- fanotify on whole filesystems.
- Answers filtered per asker (DESIGN 4).
- The sandboxed unit.
- Tests that need root run in a virtual machine, never on the
  developer's own system.

## Phase 4: Windows

- A user's engine first: the walk and `ReadDirectoryChangesW`.
- Then the service: the file table and the change journal.
- Tried under Wine as far as Wine answers.

## Phase 5: macOS

- `getattrlistbulk` and FSEvents, from fsearch.
- A LaunchAgent, and a LaunchDaemon for the system scope.

## Later

- Content search, if decided (DESIGN, open question 4).
- Packages: deb, rpm, AUR, Nix, Homebrew, winget.
- A development shell (a Nix flake) and CI once there are crates to
  check on three systems.
