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
  cancelled; nothing that answers a client waits on one. The writer
  answers syncs: it opened every root itself once, so a spinning disk
  held every root for its crawl. Each root opens on a thread of its own
  (`Writing::open_later`); what is heard for it meanwhile is held.
- **Hold nothing of a filesystem between reads.** A descriptor kept on
  a mount makes its user's `umount` fail, and a `cryptsetup close` after
  it. A root's folder is open while it is read and let go of after
  (`Root::let_go`, the keeper after each operation); a fanotify mark is
  set through a mount made for it and let go of at once; handles are
  opened through the mount table's mounts when they are read. What is
  opened again is known by its identity and mount id, never by its path
  alone.
- **A share counts all the work it bounds,** and stays under its
  budget, leaving room for what cannot wait. The writer's work for what
  is heard (finding, applying, saving) is timed by its own thread's
  clock and held to 1.5%, three quarters of the idle budget (DESIGN 8,
  "One account, sized to the budget"). Reading events, which keeps the
  rest, cannot wait without losing them. A share that counted applying
  alone took the whole 2%, and the system service took 2.8%. A share
  each folder keeps to bounds one folder, not many: what no folder's own
  hold bounds goes through one account (`live::Gather::spent`), or 30
  busy folders take 20% of a core.
- **Wake for a batch, not for each event.** A thread woken once for each
  event spends more on waking than on the event: the writer woke 3,300
  times for 3,700 events, 60% of its time. The reader waits a little
  after a read that gave events (`live::READ_EVERY`), never while idle.
- **What is found in an index is kept only while the index keeps its
  shape** (`Tree::shape`: no folder made, moved or let go of). An
  identity does not say where a folder is: a folder moved is in the
  index twice until its old parent is read again. What is found in no
  index is kept only while no index changes its shape (`tree::shapes`):
  a folder comes into one only by a change that moves it. Whatever
  finding gives is kept only while it holds (`Unfound`), but a handle
  that failed to open, which lowers coverage.
- **Watch, then open, then list.** A listing may leave out what was made
  after its folder was opened (POSIX; btrfs does): a folder is watched,
  then opened again for its listing, so that what is made meanwhile is
  listed or heard, never neither. A check watches before its stat.
- **A path compared is resolved by `skwird_crawl::canonical`,** never
  `std::fs::canonicalize`. On Windows, std gives the verbatim form
  (`\\?\C:\Users`), and clients name paths plainly (`C:\Users`): the
  two never compare equal. Names are bytes everywhere: on Windows,
  their WTF-8 (`skwird_crawl::wtf8`), never Rust's encoded bytes of an
  `OsStr`, which it does not promise.
- **On Windows, a process skwird starts inherits no handle.** Windows
  gives a child every inheritable handle its parent holds where asked
  to inherit any, and std's `Command` asks. An engine started so held
  its client's output pipe open, and what read the client's output to
  its end waited for the engine to go idle. A client starts the engine
  with `CreateProcessW` inheriting nothing (`skwird_client::pipe`), and
  names its log (`SKWIRD_LOG`), which the engine opens itself.
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
  `nix build .#checks.x86_64-linux.user-service -L` (and `system-service`,
  `root`), or its `.driverInteractive` to work in the machine by hand.
  The service's own lines go to the machine's journal, not its console:
  a test that waits on the service prints `journalctl -u ...` to see
  why it does not answer. They use KVM
  where the processor's virtualization is on; otherwise QEMU emulates
  the processor, many times slower.
- A test that needs root lives in a `tests/root*.rs` of its crate, and
  starts with `let Some(m) = skwird_testkit::machine() else { return };`:
  it runs only as root in the root tests' machine, and is skipped
  anywhere else. `tools/vm/root-tests` builds them with cargo and runs
  them there in about 15 s; the gate runs it (`--no-machine` skips it).
  The machine gives them an ext4 filesystem and a btrfs one mounted
  through a subvolume (`m.ext4`, `m.btrfs`, its subvolumes `a` and `b`).
- Windows' tests run under Wine. `tools/wine-tests`, run in the flake's
  development shell, builds the workspace's tests for
  `x86_64-pc-windows-gnu` and runs them in a Wine prefix of their own,
  under `~/.cache/skwird-tests/wine`, with a `HOME` of its own there.
  The gate runs it (`--no-wine` skips it). Never run `wine` by hand
  with the user's `HOME`: Wine links a prefix's Documents, Desktop and
  Downloads to the user's own folders. A test that passes there passes
  against Wine's answers, not NTFS's. A path in a test is absolute as
  its system has it: `/home/...` is not absolute on Windows.
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
