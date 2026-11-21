# Working on skwird

skwird is an index of file names kept current by the system's change
notifications, answered over a local socket, for Linux, macOS and
Windows. This file is what every contributor, human or model, needs
before touching the code. Read `docs/DESIGN.md` (the decisions and why)
and `docs/PLAN.md` (the phases and where they stand) first.

## Privacy and fixtures

- An index of names is a list of what is on someone's disk. Never put
  personally identifying information in the repository: tests,
  documents, captured output, commit messages. Paths and file names
  from a report are private input, not fixtures.
- Reproduce with synthetic names that keep the property that matters
  (spaces, Unicode, depth, links, hard links) in temporary folders.
  Never hard-code a real home folder.
- Output meant to be shared (`skwird-bench`, logs a user may paste)
  says counts, sizes and times, never a name or a path.

## Binding rules (docs/DESIGN.md)

1. **Coverage is never overstated.** An answer claims live or checked
   only when a mechanism proves it. Any doubt (an overflow, a failed
   watch, a new mount, a folder denied) lowers it at once, for the
   subtree concerned. A test proves each way coverage can be lost.
2. **A system service answers only what the asker could list,** and
   its index is readable by it alone.
3. **Private folders are never indexed,** not merely left out of
   answers.
4. **One engine per scope.** Clients speak the protocol; nothing that
   watches runs twice for one user.
5. **No types, no query language in the protocol.** Clients type files;
   queries are data.
6. **Idle is idle and nothing leaves the machine.** No polling loop, no
   network, ever.
7. **Every system.** Code only one system has sits behind `cfg`, with
   the others' answer beside it: their code, or an error naming the
   system, never Linux's behaviour where it would be wrong.

## Engineering

- **Rust 2024.** The workspace's lints are pedantic clippy, and
  `cargo clippy --all-targets -- -D warnings` must pass. An allowance
  in code says why in a comment.
- **Unsafe.** The workspace denies it. A module that speaks to the
  system (fanotify, FSEvents, Win32) allows it at its top, with a
  `SAFETY:` comment on every block. Use rustix where it has the call.
- **Blocking.** A stat can block on a sleeping disk or a dead network
  share: crawls and checks run on threads of their own and can be
  cancelled; nothing that answers a client waits on one.
- **Locks and pools.** Never hand work to a thread pool while holding a
  lock or initialising a once-cell. A pool thread that waits on it can
  pick up another task that waits on the same, until every thread is
  parked: fsearch 0.2 deadlocked so.
- **Fix structurally.** When a symptom returns or a fix needs
  hand-kept constants, find the invariant being broken, make it
  impossible, and write the rule here.
- **What follows from a list is derived from it again.** It is not kept
  up by the events that changed the list alone: an event can come while
  no engine runs, or be cut short by a kill. The folders an index holds
  as excluded are derived from the exclusions at each open and each
  change (DESIGN 5).
- **Decisions.** A choice that closes a door is recorded in
  `docs/DESIGN.md` with its reason; `docs/PLAN.md` says honestly what
  each phase did and did not do.

## Testing

- `cargo test`. Tests that need a real filesystem's behaviour (change
  times, hard links) use a scratch folder under the user's cache
  (`~/.cache/skwird-tests/`), never `/tmp`, which may be a tmpfs.
- Tests that need root (fanotify, a system service) run in a virtual
  machine, never on a developer's own system. The flake's `checks` are
  those machines (`tools/vm/`, NixOS's test framework, QEMU):
  `nix build .#checks.x86_64-linux.user-service -L`, or its
  `.driverInteractive` to work in the machine by hand. They use KVM
  where the processor's virtualization is on; otherwise QEMU emulates
  the processor, many times slower.
- Assert what an answer holds, not merely that it parses.

## Shell hygiene

- Never put backticks in a double-quoted `git commit -m "..."`: write
  the message to a file and use `git commit -F`.
- `pkill -f pattern` matches the shell running it; stop a process by
  its pid.
- Chain checks with `&&` before a commit so a failing one stops it.
- `a && b && c &` sends the whole chain to the background, its `export`
  with it: what follows in the foreground runs without them. Set a
  process's environment on its own command line (`VAR=value cmd`).
- **An engine run by hand or by a test never reaches its user's own.**
  Give every `skwird` it runs, and every client that may start one,
  `XDG_RUNTIME_DIR`, `XDG_CACHE_HOME` and `XDG_STATE_HOME` of a scratch
  folder, on its own command line (a test: `Command::env`); a test
  gives `HOME` and `XDG_CONFIG_HOME` too. A measurement of a real home
  keeps `HOME`, so that the default exclusions name the user's own
  private folders, and sets `SKWIRD_NO_START`, so that no client of it
  starts another engine. Otherwise a client starts the user's engine,
  which crawls the whole home into the user's cache: it happened once,
  in phase 1's step 5, and again in phase 3's first step, from a
  package's `skwird status` run for a glance. Any `skwird` is such a
  client, a build's or a package's, whatever it is run for.
