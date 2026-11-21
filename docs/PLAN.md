# skwird's plan

Each phase is useful alone, and ends with what was measured and what is
left, honestly. The design is `docs/DESIGN.md`; a phase that changes a
decision changes it there.

## Where it stands (2026-11-21)

Phase 0 is done. Phase 1 is planned below and not started. The choices
it waited on were made on 2026-11-11 (DESIGN 2, 5 and the Protocol):
private folders a client registers, tool stores indexed, network
mounts never crawled, no crate on crates.io. skwir released 0.6.0 on
2026-11-21, and skwird's steps are next in its plan (below, "skwir, the
first client").

## Phase 0: measure (done)

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
  - `skwird-crawl`: the Linux walker and the check, with the mount
    table. Other systems get an error naming them (rule 7);
  - `skwird-index`: the format, its chunks rewritten copy-on-write, the
    log, and queries on them, with fsearch 0.2's packed arrays and
    zones (DESIGN 8);
  - `skwird-protocol`: the messages, which the engine speaks too;
  - `skwird-client`: a client and a fake engine for clients' tests.
    Like `skwird-protocol`, it depends on serde and serde_json alone,
    so their copies build on their own (DESIGN, Protocol);
  - `skwird`: the program, which serves and is the command line. It is a
    library too, so the engine is tested without a socket.
- **Roots:** the user's home, and folders the user's configuration
  names. A client cannot add one (DESIGN 5). Beyond them, an answer is
  not covered: outside the engine's roots. A mount below a root is not
  crawled in this phase. Its answer is not covered: a mount, or a
  network mount (DESIGN 2). Crossing into a local mount, with an index
  file of its own, comes later.
- **Coverage:** names checked at T at best, attributes as of each
  folder's last read. `sync` runs the check (DESIGN 2).
- **Protocol 0**, experimental.
- **Accepted:** an engine started by `skwir index` under its timer ends
  with that run (DESIGN 1). The next client starts another and pays one
  check. Phase 2's unit removes this.

**The gate,** `tools/gate.sh`, chained before every commit: `cargo fmt
--check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and
the same clippy for `x86_64-pc-windows-gnu` and `aarch64-apple-darwin`
(rule 7).

**Steps.** Each ends when its tests pass and the gate holds, and is
committed then.

1. **The test kit, then the crawl.**
   - The kit comes first: a builder of synthetic trees under
     `~/.cache/skwird-tests/`, and a plain reference walk to compare
     with. The trees have spaces, Unicode, names that are not UTF-8,
     more than 256 levels, links, hard links, a btrfs subvolume, and a
     tmpfs and a bind mount in a user namespace of the test's own.
     Steps 2, 3 and 5 all test "equals a fresh walk" with it.
   - The bench's walker and check move into `skwird-crawl`, and the
     bench depends on it, so what is measured is what is built.
   - Added: reads through the parent's handle and by `openat2` with
     `RESOLVE_NO_SYMLINKS`; a failed or cut-short listing not covered;
     the mount table and mount ids; `.private` and the default
     exclusions resolved to folders; a folder read within a tick read
     again; cancellation.
   - The toolchain moves to skwir's (1.99.1), so the copies of the
     client's crates meet the lints skwir runs.

   **Done on 2026-11-21.** `skwird-testkit` (`Scratch`,
   `reference_walk`, `in_namespace`, which runs a test again under
   `unshare --user --map-root-user --mount`) and `skwird-crawl`
   (`crawl`, `read_folder`, `check`, `mounts`, `exclude`). The crawl
   lists what the reference walk finds, entry for entry, on a tree of
   awkward names 300 levels deep. Of the fixtures below, those for the
   crawl pass, the namespace's and the subvolume's run here rather than
   skipping: times set back, the tick, a folder or one above it swapped
   for a link, denied then readable, a listing cut short (from a listing
   of the test's making), deeper than the system resolves, exclusion
   spellings, `.private`, a subvolume, a tmpfs and a bind mount, a mount
   made after the crawl, a network mount (from a table of the test's
   making). The bench walks and checks with it. On a synthetic tree of
   1,011,101 entries in 11,111 folders, warm, 20 threads, best of six
   runs alternated with the walker it replaces, on a loaded machine:
   every entry stat-ed, 417 ms against 403; names alone, 54 ms against
   49.
   - Not done here: a folder's ACL flag (phase 3 needs it); what a
     root or a folder on FUSE can claim, whose change times do not tell
     (step 3: never "checked").
2. **The index.**
   - A spike first, on the format's one hard part: a subtree stays one
     range while chunks are rewritten. Its answer shapes the format, so
     it comes before the format is written. Done on 2026-11-21: yes, by
     pre-order with depths and new folders after their parent's block
     (DESIGN 8, "How a subtree stays one range"), held through 240,000
     random edits. It does without an overlay.
   - The crawl's output laid out depth first, names stored once, packed
     arrays and zones, written to a file and mapped. Queries by name
     (matched as skwir matches, DESIGN 7), kind, size, time and root.
   - Done when every query equals a filter over a fresh walk, and the
     size is measured against the estimate of 59 bytes per entry.

   **Done on 2026-11-21.** `skwird-index`: packed columns in runs of 64
   (`packed`), chunks of 31 columns with zones of 1,024 entries
   (`chunk`), the names each once with a mask of what they hold
   (`names`), names matched as skwir matches them (`matching`, held to
   skwir's rules on terms that trip Unicode up: `İ`, the Kelvin sign,
   `ſ`, a final sigma, bytes that are not UTF-8), the build from a
   crawl, queries, and a store whose files are written once and mapped
   back (`store`, `map`). Every query of a test tree (names, kinds,
   sizes, times, mixed, below seven folders) equals a filter over a
   fresh walk, in one chunk and in many, built and mapped back; a
   damaged chunk, a manifest of another layout and a missing index are
   refused. `skwird-bench index` measures it. On a developer's work
   folder (2,882,900 entries in 233,322 folders, 711,496 distinct
   names; btrfs, warm, 20 threads):
   - **Size:** 84.4 MB, 29.3 bytes an entry (chunks 53.8 MB, names
     30.6 MB): 63% of the estimate at full widths for that tree (46.8
     bytes an entry). The home phase 0 measured would take about
     215 MB if it packs alike; its names are a larger share (44% of
     its entries are distinct, against 25% here).
   - **Times:** crawl 1.5 s, build 1.8 s, save 0.7 s, load (mapped and
     every file checked) 0.23 s. Queries after the load: a name
     matching one entry 5.5 ms, `report` 11 ms (1,558 hits), `*.pdf`
     71 ms (142,746 hits), `e` about 1 s (1.38 million hits: building
     each hit's path is most of it).
   - Not done here: **memory.** The first build peaked at 1.2 GB, the
     crawl held whole before the blocks are made, and the blocks whole
     before they are cut. Step 5 streams the crawl into the build. A
     hit's path costs about half a microsecond: answers stream in
     batches with a limit (step 5).
3. **Coverage.**
   - Load ("as of"), check, reread, and the diff into the chunks it
     touches. Coverage per root, each subtree not covered with its
     reason.
   - A randomized test against a fresh walk. Random edits while the
     engine is down and while it runs: make, rename, swap for a link,
     remove, deny, add `.private`. After each, the index equals the
     walk, and the coverage claims no more. Phases 2 and 3 reuse it.

   **Done on 2026-11-21.** `skwird_index::update`: `refresh` (the
   mount table read again, the check, each stale folder read again and
   put in place by `Index::reread`, each new folder crawled by
   `Index::graft`), and `Index::coverage` (the names' proof, the time
   the attributes are as of, each topmost hole with why). A folder kept
   by identity keeps what is below it; one gone or replaced takes its
   subtree. What failed stays as read and is a hole; what is private,
   excluded or under a mount is let go of at once. Names given since a
   save are a segment of their own, saved as one more file: a reread
   reuses its folder's names. `skwird_crawl::Why` gained `Unread`.
   - **Fixtures** pass: a change while the engine was down (saved,
     changed, loaded as of its save, then checked), a folder that gains
     entries then is removed or replaced by a file, denied then
     readable, made private then not, a listing cut short (kept as
     read), a mount made below the root then let go of (in a namespace
     of the test's own).
   - **Random edits** (made, removed, moved, swapped for a link, denied
     and allowed, `.private` added and taken, several while the engine
     is down, some across a save and a load): 150 seeds of 60 refreshes
     held, with the tick and with a tick of zero, where the comparison
     of change times alone finds what changed (sound on Linux 6.13,
     whose change times are fine once observed). The test requires its
     edits to make holes, denied and private among them.
   - Not done here: a root on FUSE, whose change times do not tell, is
     never "checked" (`refresh` says "as of"), but no test makes one.
     Attributes are as of the last whole crawl: rereads do not advance
     that time.
4. **The log.**
   - Appends, and background merges of the chunks touched.
   - The journal (`changes`, DESIGN 8).
   - A failed save that keeps the index and its "as of"; a damaged or
     older file rebuilt; a folder made private, or registered by a
     client, purged.
   - Done when the kill and save-failure tests pass.
5. **Protocol 0 and the engine.**
   - The socket in `$XDG_RUNTIME_DIR/skwird/`, peer credentials
     checked: hello, coverage, sync, query, cancel, changes, exclude.
     Names that are not UTF-8 travel byte for byte.
   - The lock and its takeover; the client starts the engine; the idle
     minute.
   - `skwird-protocol` and `skwird-client` stand alone.
     `tools/vendor-client` writes their copies, and a test builds those
     copies outside the workspace. Transcripts of the messages are kept
     as test data, for clients that write their own.
6. **The command line and the budgets,** the one step that needs the
   user.
   - `skwird find`, `coverage` and `status`; `tools/measure.sh` gains
     the engine.
   - The user runs it on their home. Memory, the time to the first
     answer and idle CPU become budgets (DESIGN 10), and the index's
     size is compared with the estimate.

**Tests:** coverage is never overstated. Fixtures cover:
- a change made while the engine was down;
- a folder whose times `tar` set back;
- a folder changed again within a tick of being read;
- a mount below the root, a bind mount of the same filesystem (made in
  a user namespace), and a reread that starts at a mount: none crossed;
- a network mount below the root: never entered, not covered: network.
  Told from the mount table's text, as skwir's test tells it;
- a btrfs subvolume below the root: walked, with a device of its own;
- a folder denied, then made readable: not covered, then read at the
  next check;
- a listing that fails part-way: not covered, never empty;
- a name added during the crawl;
- a folder, or a folder above it, replaced by a link: another folder's
  names never appear under its path;
- a folder that gains entries, then is removed or replaced by a file:
  nothing under it remains;
- a tree deeper than 256 folders, its paths longer than the system
  allows;
- an exclusion written with a trailing slash, a doubled slash, or
  through a link: it excludes its folder, and only it;
- a folder a client registers after it was indexed: purged, its log
  entries too, and not covered: excluded;
- a token from before a rebuild: too old.

**Tests of the engine:**
- after every batch of changes, each entry's parent is a folder in the
  index;
- queries made at once answer as queries made one by one;
- the first engine exits during its first crawl, and the second takes
  over;
- a save that fails keeps the index, and its "as of";
- a socket path too long is an error that says so;
- a sync after a change made through the engine's back answers with
  the change;
- two clients register folders, and each removes only its own.

**Done when:** a name query of a home tree gives what a walk would,
after a restart as well; and the memory, the time to the first answer,
idle CPU and the index's size (against `skwird-bench`'s estimate) are
measured, and the first three become budgets.

## skwir, the first client

skwir's steps are in its own `docs/PLAN.md` ("Names indexed by skwird",
its ADR-050). Its sessions build them, in its repository:

- **A. Coverage in its hints.** `HintProvider` says live, checked, as
  of, or not covered and why, for a root and the facts a search needs.
  It needs nothing from skwird, and is tested with a fake provider. It
  can start now.
- **B. A skwird provider,** after step 5. skwird's
  `tools/vendor-client` writes the copies. skwir's session commits them
  as `vendor/skwird-protocol` and `vendor/skwird-client`, with their
  `README.skwir.md`, and takes them by path, outside its workspace as
  `vendor/zed` is. It also amends its ADR-050, whose point 7 waited for
  a crate on crates.io. The copies' two dependencies come from
  crates.io through skwir's lockfile, and their advisories are checked
  as skwir's own.
- **D. The harvest's enumeration and journal** (skwir's ADR-089):
  `changes` from step 4, live from phase 2.

Kept alike in both: the four coverage words; name matching (DESIGN 7),
so a change to one is a change to the other; and how a network mount
is told (DESIGN 2).

## Phase 2: Linux, a user service

- A systemd user unit. It outlives a client's run, which removes phase
  1's accepted limit. Socket activation where a package installs it.
- inotify, read through rustix, for the folders clients ask for:
  skwir's harvest scope, the folders open in its panes. Within a share
  of the user's watch limit, with the check covering the rest (DESIGN
  3). A wrapper can pass an overflow on as an ordinary event: `notify`
  does, and fsearch-crossplatform misses it.
- Each folder watched before it is read or checked (DESIGN 2).
- The sync barrier (cookies).
- The journal live: changes heard, not only found by a check.
- The mount table followed, as skwir follows it.
- Tests, without root: an overflow (a queue left unread while more
  files are made than `max_queued_events`) lowers coverage at once; a
  change made while the watches are being installed is not missed.

## Phase 3: Linux, the system service

- fanotify on whole filesystems. On btrfs, use a detached read-only
  top-level mount when marking through a subvolume is refused (DESIGN 3).
- Coverage tests for queue overflow, mount and subvolume changes, and
  handle resolution failures; phase 0 establishes feasibility only.
- Answers filtered per asker (DESIGN 4).
- Each user's registered folders kept per user, each applying only to
  folders that user owns (DESIGN 5).
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

- Packages, beside skwir's: skwir.org's apt and dnf repositories, the
  AUR, a Nix package in nixpkgs' manner (as skwir's ADR-124), Homebrew,
  winget. Whether the name is free there is DESIGN's open question 9.
- CI once there are crates to check on three systems. The development
  shell (`flake.nix`) already carries the Windows and macOS targets.
