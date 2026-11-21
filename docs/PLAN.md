# skwird's plan

Each phase is useful alone, and ends with what was measured and what is
left, honestly. The design is `docs/DESIGN.md`; a phase that changes a
decision changes it there.

## Where it stands (2026-11-21)

Phase 0 is done. Phase 1 is done, its eight steps: a user's engine on
Linux, started by its first client, answering protocol 0, measured on
the user's home and given budgets (DESIGN 10), the three of memory
(after a first build, at its peak, after a sync) met at the second and
third measurements. The
choices phase 1 waited on were made on 2026-11-11 (DESIGN 2, 5 and the
Protocol): private folders a client registers, tool stores indexed,
network mounts never crawled, no crate on crates.io. skwir released
0.6.0 on 2026-11-21, and its steps for skwird (below, "skwir, the first
client") can take the client's copies now.

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

## Phase 1: Linux, a user's engine, started by its first client (done)

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

   **Done on 2026-11-21.** `skwird_index::log` (records with checksums,
   tokens), `skwird_index::keeper` (opened from disk with its log
   replayed, or built again with why; refreshed, each change logged;
   saved; asked `changes` below a folder since a token; purged),
   `Index::replay`, `Index::blank_dead_names`, the manifest's third
   version (the log's epoch and how far the chunks go into it, when the
   names were last proven). Tests pass: an index dropped unsaved opens
   again with its log replayed, as of its last proof, equal to the tree;
   the journal gives each change below a folder with identity and kind,
   a removal with what it was, and "too old" for a token of another
   epoch or ahead of the log; a save that fails (its folder read-only)
   keeps the index in memory and the manifest as it was, and what is on
   disk opens to the refresh; a purge leaves no byte of the folder's
   names in any file; a damaged chunk is built again, with why; a log
   whose tail is torn or flipped ends there. **The kill test**: a
   process editing, refreshing and saving, killed at random moments,
   120 rounds, mid-append and mid-save: each time what it left opened
   (all 120 loaded, none built again) and refreshed equal to the tree.
   - Not done here: **saves run on the writer's thread**, not in the
     background. There are no merges to make: a change rewrites its
     chunk in memory at once, and a save writes only what is new. The
     engine (step 5) decides when to save.
   - A crash during a large graft (a big folder copied in) replays it a
     folder at a time, each rewriting its chunk: correct, slow. The
     journal keeps what its 16 MB of log hold.
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

   **Done on 2026-11-21.** `skwird-protocol` (the messages, JSON lines,
   a path not UTF-8 as its bytes, a session of every message in
   `transcripts/`), `skwird-client` (finding an engine, starting one in
   a process group of its own, asking it; `fake`, an engine of a test's
   making), and `skwird` (the engine: the socket in a folder its user's
   alone, each peer's credentials checked, the lock and its takeover, a
   writer thread owning each root's keeper and publishing snapshots,
   syncs gathered into one refresh, a query on a thread of its own, the
   idle minute; and `find`, `coverage`, `sync`, `status`).
   `tools/vendor-client` writes the copies with manifests of their own:
   `cargo package` would have turned the client's path to the protocol
   into a version, which crates.io does not hold. Tests pass, each
   process given a scratch home and XDG folders: the engine answers what
   a walk finds, names not UTF-8 byte for byte; a sync answers with a
   change made behind its back, checked at a time after it was asked;
   queries at once by six clients answer as one by one; a second engine
   blocked on the lock takes over when the first is killed during its
   first crawl; a socket path too long is an error that says so; two
   clients register folders and each takes back only its own; a query
   cancelled after its first batch says so and stops; an engine goes
   once idle, its socket let go of; a client starts the engine where
   none answers; the copies build alone, offline. After every refresh of
   the random edits (step 3), the index is checked whole: each block in
   pre-order below its parent, each folder entry naming its block.
   - **An incident, and its rule.** A run by hand put an `export` of the
     scratch folders before a `&`, which took it to the background: a
     `skwird status` then ran with the user's own folders, found no
     engine, and started one, which crawled the user's home into the
     user's cache (6.9 million entries, 274 MB). It was stopped, and
     everything it wrote removed. AGENTS.md, "Shell hygiene", now says
     how every run is kept to its own folders.
   - **A gap found after, and closed** (2026-11-21, from the review of
     fsearch 0.2.2). Exclusions were applied as events only: a client's
     registration purged its folder, and a withdrawal read it again.
     Nothing held a loaded index to the list. A line added to the user's
     file, while an engine ran or while none did, left the folder's
     names indexed and answered until its parent was next read: rule 3
     broken. A purge or a readmission cut short by a kill was lost, and
     a folder taken back while no engine ran stayed excluded for good.
     Now the keeper derives the excluded folders from the list at each
     open and each change, and the engine reads the user's file again
     at each sync (DESIGN 5). Tests pass, each failing without the fix:
     - an exclusion added while no engine ran holds from the next open,
       no byte of the folder's names left on disk;
     - one taken back is read at the next refresh, even with a kill
       between;
     - the engine purges a folder named in the user's file from the next
       sync, and, written while no engine ran, before its first answer.
   - Not done here: **the crawl is not yet streamed into the build**:
     the first build of a large home holds the crawl whole (1.2 GB for
     2.9 million entries in step 2). Step 6 measures it against the
     budgets. A query's answers are gathered before the first batch is
     sent, so `cancel` saves the sending, not the scan. A client's path
     is matched to a root by its spelling: a path through a link is not
     resolved.
6. **The command line and the budgets,** the one step that needs the
   user.
   - `skwird find`, `coverage` and `status`; `tools/measure.sh` gains
     the engine.
   - The user runs it on their home. Memory, the time to the first
     answer and idle CPU become budgets (DESIGN 10), and the index's
     size is compared with the estimate.

   **Ready on 2026-11-21, waiting for the user.** `skwird find`,
   `coverage`, `sync` and `status` came with step 5. `tools/measure.sh`
   gained two steps:
   - `index`: `skwird-bench index`, the default exclusions honoured: the
     crawl, the build, the size against the estimate, a save and a
     load, nine queries timed, the peak memory.
   - `engine`: `skwird serve` on the folder, its runtime, cache and state
     folders the measurement's own, `HOME` the user's (so the default
     exclusions are theirs) and `SKWIRD_NO_START` set: the time to the
     first answer with the index built and with it read back, the
     memory then, a warm sync, the processor time of 30 idle seconds.

   Both print counts, sizes and times alone, and remove what they built
   (the index of the folder measured), even when cut short. Tried on a
   synthetic tree of 1,530 entries: the engine answered first in 0.07 s,
   held 5 MB, and took no processor time in 30 idle seconds.

   **Done on 2026-11-21.** The user ran `tools/measure.sh` on their home
   at `8850b44`: 6.9 million entries in 867,000 folders, 3.0 million
   distinct names, on btrfs on a solid-state disk, 20 threads, 32 GB.
   The budgets are in DESIGN 10, with what met them and what did not.
   Beside them:
   - **The index:** 286 MB on disk, 41.6 bytes an entry, 70% of
     `skwird-bench`'s estimate (63% on step 2's tree, whose names repeat
     more): chunks 141.5 MB, names 144.9 MB. Built in 4.2 s, saved in
     0.69 s, mapped and checked in 0.69 s.
   - **Queries,** the index read back:
     - a name with 12 matches in 10 ms; "report" (4,274) in 30 ms;
     - `*.pdf` (144,028) in 107 ms; every folder (866,580) in 0.54 s;
     - a name in most entries (4.1 million) in 1.7 s.

     Past the scan, a hit costs about 0.4 µs, since a query gathers its
     hits whole before its first batch (step 5).
   - **The engine:**
     - Its first answer, building, came at 9.3 s with the caches warm.
       The bench's crawl before it took 15.3 s with the files' inodes
       mostly cold: the check before it had dropped the caches, and
       stats folders alone. A walk with every entry stat-ed takes 17.7 s
       cold and 4.1 s warm.
     - A sync's refresh took 0.66 s, of which the check (by path, 20
       threads) is 82 ms. The rest is the engine's: each folder's path
       and stored stat, the comparison. That is within the budget, but
       it is where a sync's time goes.
     - Read back, it replayed no record and answered in 0.62 s, at
       177 MB. Idle, it took no processor time in 30 s.
   - **As in phase 0:**
     - A watch per folder would take 866,584 of the user's 524,288
       inotify watches.
     - On btrfs, the fanotify mark through the top-level subvolume was
       accepted, and every probe was heard.
7. **The memory budgets** (DESIGN 10): the two the measurement found
   not met.
   - **The first build streamed from the crawl,** the crawl never held
     whole: at most 150 MB per million entries at the peak, where
     329 MB were measured (step 5 said why).
   - **After the first build, back to the memory of an index read
     back.** The chunks and names built are mapped from their files
     once saved (`store::save`), and yet the engine stayed at 1,873 MB,
     against 177 MB read back. What holds it is found first, then fixed
     by its cause. The likely one is the allocator keeping what the
     crawl's 20 threads freed.
   - Done when `tools/measure.sh --only engine` on the user's home meets
     both.

   **Built on 2026-11-21, waiting for the user's measurement.**
   - **What held it, found first.** On a synthetic tree of 1.04 million
     entries in 70,481 folders, a first build left the engine at
     336 MB, 322 MB of it anonymous, against 19 MB read back.
     `malloc_trim` took it to 64 MB. So the memory was the crawl's and
     the build's many small allocations, freed and kept by glibc's
     allocator. The index itself was not held.
   - **The packer** (`skwird_index::packer`): each folder the crawl reads
     is packed at once, on a thread of its own fed by the crawl's
     threads, its listing let go of. It goes into a few buffers that
     only grow:
     - the names, each once, found by their hash;
     - each folder's block, its numbers as variable-length integers;
     - by slot, where each block starts.

     Once the crawl ends, the blocks are unpacked in pre-order, each
     folder's folders linked by name, and cut into chunks as
     `Tree::build` cuts them. `build` and the keeper's first build both
     go through it, and `skwird-bench index` builds as the engine does.
   - **What is left given back:** the engine calls `malloc_trim` once,
     after a first build, where the C library is glibc (`memory.rs`,
     the one call to the system it makes for this).
   - Measured on that tree, by the kernel's count:
     - after the first build: 18 MB, where 336 MB were, as against 19 MB
       read back;
     - at the peak: 67 MB, where 340 MB were;
     - the build: 0.42 s, as before (0.45 s).

     That is 18 MB and 65 MB per million entries, against the
     budgets' 40 and 150.
   - Tests pass:
     - the packer's blocks, and its chunk cuts, are those `blocks_of`
       and `Tree::build` make, on folders of every size, one past a
       chunk's most, one empty, one excluded;
     - names whose hashes are alike are two names;
     - every test of the index and the engine, which build through it.

   **Done on 2026-11-21.** The user ran `tools/measure.sh --only engine`
   on their home at `bab36bc` (6.9 million entries in 867,128 folders):
   - after the first build, 253 MB, where 1,873 MB were: 37 MB per
     million, within the budget of 40;
   - at the peak, 706 MB, where 2,268 MB were: 102 MB per million,
     within the budget of 150;
   - the first answer, building, at 5.6 s, where 9.3 s were: the crawl
     and the packing overlap;
   - a warm sync in 0.65 s, where 0.83 s were;
   - read back: 178 MB, a first answer in 0.57 s; idle, no processor
     time.

   Its folders hold fewer entries, and its names repeat less, than the
   synthetic tree's, hence 37 MB per million after a build where the
   synthetic tree took 18.

   **Found by it:** idle after a sync, the engine held 384 MB, 56 MB
   per million, over the at-rest budget. On the synthetic tree, a sync
   adds about 20 MB of anonymous memory for 70,481 folders, whether the
   index was built or read back. A second sync adds about 1 MB, and the
   mapped files do not change. So it is the refresh's own allocations,
   freed and kept: the check's list of every folder, each with its path
   in an allocation of its own. The first measurement could not show
   it, since the allocator held 1.9 GB of freed memory for the refresh
   to take from. Step 8 is to meet it.
8. **The memory of a sync** (DESIGN 10): idle after a sync, at most the
   at-rest budget, 40 MB per million entries (56 measured).
   - The check's list of folders without an allocation for each: their
     paths in one buffer. What a check is handed (`skwird_crawl::Stored`)
     changes with it.
   - Then measured: if what is left is still over, it is given back
     after a refresh, as after a first build (`memory.rs`).
   - Done when `tools/measure.sh --only engine` on the user's home
     meets it.

   **Built on 2026-11-21, waiting for the user's measurement.** All on
   the synthetic tree of step 7 (1.04 million entries in 70,481
   folders), anonymous memory read in the engine's own mappings:
   - **The list in one buffer.** `Stored` keeps every path end to end in
     one buffer, and each folder's identity, change time and flag in
     another, reserved at once (about 64 bytes a path). `Index::stored`
     makes each path in a buffer kept for its depth. A sync then added
     8 to 13 MB where it had added 18 to 23 MB. Not enough.
   - **What was left, found.** With nothing changed, syncs still settled
     at 7 MB more than at rest, and `malloc_trim` after each did not
     take it back. Read by mapping:
     - 21 thread arenas of 132 KB each, the check's threads': a constant;
     - one arena heap of 8.9 MB, 5.2 MB of it resident, which the
       writer's arena had grown to.

     glibc gives an allocation a mapping of its own past a threshold,
     which rises each time such a mapping is freed: here the load had
     freed one. The next list then came from the writer's arena, and
     `malloc_trim` never shrinks a thread arena's top.
   - **The thresholds fixed** as the engine starts (`memory::settle`,
     `mallopt`). Every allocation of 1 MiB or more has a mapping of
     its own, given back whole when freed, and a heap's free top is
     given back past 1 MiB. What is free is given back after each
     refresh, as after a first build.
   - Measured with the threshold at 128 KiB, as first set, five syncs
     after each start, one with a change made behind the engine's back:

     | | ready | after the syncs |
     |---|---|---|
     | built, 20 threads | 18.4 MB | 18.4 to 19.0 MB |
     | read back, 20 threads | 19.0 MB | 21.7 MB |
     | read back, 2 threads | 19.0 MB | 19.1 MB |

     The 2.7 MB with 20 threads is the arenas', whatever the tree's
     size. The build's peak (66 MB) and time (0.46 s) did not change.
   - Tests pass: the paths given to the list read back each its own, the
     root's empty one, names not UTF-8 and a path past the system's
     limit among them. Every test of the check, the refresh and the
     engine takes the list.

   **Done on 2026-11-21.** The user ran `tools/measure.sh --only engine`
   on their home at `7f98a7d` (6.9 million entries in 867,166 folders):
   - idle after a sync, 144 MB, where 384 MB were: 21 MB per million,
     within the budget of 40;
   - after the first build, 144 MB, where 253 MB were; at the peak,
     598 MB, where 706 MB were;
   - a warm sync in 0.43 s, where 0.65 s were: no path is made an
     allocation of its own any more;
   - read back: 178 MB, a first answer in 0.63 s; idle, no processor
     time.

   **The threshold, then raised to 1 MiB.** The first answer when
   building came at 8.6 s, where the second measurement's came at
   5.6 s. On the synthetic tree, the same build ranged from 0.59 to
   0.96 s over runs with nothing changed, so one run says little of it.
   What the threshold cost was counted instead: 46,000 page faults where
   18,000 were, every buffer of 128 KiB or more taking pages fresh from
   the system. At 1 MiB, the faults are 18,400, and the memory after
   syncs is the same: 18.3 MB built, 21.8 MB read back with 20 threads.
   The large buffers, the check's list and the packer's, are past 1 MiB
   on any tree where they matter. Only the synthetic tree measured that
   change.
 Fixtures cover:
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
- a token from before a rebuild: too old;
- an exclusion added, or taken back, while no engine ran or with a kill
  between: held from the next open.

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
- two clients register folders, and each removes only its own;
- a folder the user's file names is purged from the next sync, and,
  written while no engine ran, before the first answer.

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
  `changes` from step 4, live from phase 2. A `changes` that a check
  found holds no write made in place: such a write changes no folder's
  time. fsearch's recovery missed files that grew so (its `8e6db08`).
  Live events, from phase 2, hold them.

Kept alike in both: the four coverage words; name matching (DESIGN 7),
so a change to one is a change to the other; and how a network mount
is told (DESIGN 2).

## Phase 2: Linux, a user service

The engine as a systemd user unit, outliving a client's run (phase 1's
accepted limit), with inotify watching the folders clients register.
There, its answers are live; elsewhere, checked as in phase 1. The
choices it makes are in DESIGN 2, 3 and open question 2; each step is
committed once the gate holds.

1. **The watcher** (`skwird_crawl`, Linux).
   - An inotify instance, read through rustix, with its generation: an
     instance made again leaves the old one's events applied, but
     proving nothing (DESIGN 2).
   - A folder watched through its open handle before its entries are
     read, so that the folder watched is the folder read, and a change
     between the two is heard.
   - Events named by folder (the watch descriptor's), an overflow and a
     watch that fails reported as such: a wrapper can pass an overflow on
     as an ordinary event, as `notify` does, and fsearch-crossplatform
     misses it.
   - Within a budget of watches: past it, a folder is not watched, and
     says so.
   - Tests, without root:
     - a change in a watched folder is heard, named by its folder;
     - a change made while the watches are being installed is not
       missed;
     - an overflow (a queue left unread while more files are made than
       `max_queued_events`) is reported;
     - an instance made again while the old one's events are still read:
       its events carry the old generation;
     - a budget of watches reached is reported, never a folder silently
       unwatched.
2. **Churn, measured before it is decided.** Every change is logged
   (DESIGN 8), so a build's output folder or a cache that changes without
   pause fills the log. Past 16 MB, the log ends its epoch, and clients'
   tokens turn "too old".
   - `skwird-bench churn FOLDER`: the folder watched, and what a busy
     time makes of it counted: events, folders changed, rereads, and the
     log's growth, applied at once and in batches.
   - Measured on a large build into a scratch folder of the tests'.
   - fsearch 0.2.2 indexes such folders whole, but applies their changes
     in batches: after 5 s of quiet, or 60 s at most, rereading the whole
     folder past 2,048 folders changed. If skwird does so:
     - a pending subtree is "as of" its first pending change, never live;
     - a sync applies what is pending below the path asked;
     - the folders treated so come from the index (a name query, such as
       `CACHEDIR.TAG`), never from a set learned beside it. fsearch's
       learned set was empty after a restart, and grew without end (its
       `b98a7b0`, `6c9dea4`).
   - Decided in DESIGN 8 from the numbers.
3. **Live folders in the engine.**
   - Protocol: `watch {add, remove}`, the folders a client wants live,
     kept per client across restarts as its exclusions are (DESIGN 5),
     and `skwird watch`.
   - The writer applies what is heard: each folder an event names is
     read again, events gathered, as the check reads it. The journal is
     live: changes heard, not only found by a check.
   - Coverage: live since the time a registered folder's subtree was
     wholly watched and checked, for names and attributes. A folder
     past the budget is checked, not live: a watch limit reached.
   - After an overflow, or a watch that fails, coverage is lowered at
     once, and the check runs once the queue is quiet, backing off under
     churn that lasts. fsearch 0.2.2 waits 2 s, doubling up to 60 s, and
     relists at most every 300 s. A client's sync still runs the check
     at once.
   - Tests: coverage is never overstated (below).
4. **The sync barrier** (open question 1). A cookie made in a folder of
   the engine's own, watched by the same instance, so in the same queue:
   heard, every event before it has been read. A sync of a live folder
   answers once the cookie is heard and what came before applied: live,
   up to a time after the sync was asked. Elsewhere, the check, as in
   phase 1.
5. **The mount table followed,** as skwir follows it (`POLLPRI` on
   `/proc/self/mountinfo`): a mount made or taken below a root lowers
   coverage at once, for its subtree.
6. **The user unit.** `dist/systemd/skwird.service` and `skwird.socket`:
   the engine for the session, idle never ending it. Socket activation
   (`LISTEN_FDS`) where a package installs the socket. Tested with
   `systemd-socket-activate`; no test installs a unit on the developer's
   own system.
7. **Measured,** by the user, on their home: `tools/measure.sh` gains a
   live step (memory and processor time with the watches of a large
   folder, idle with them, the time from a change to its answer).
   Budgets added to DESIGN 10.

**Tests of live coverage:**
- an overflow lowers coverage at once, and the check after it finds
  what the dropped events held;
- a folder made, filled and moved within a watched subtree is answered
  live, its new folders watched before they are read;
- a folder moved out of a watched subtree and back: nothing of it under
  its old path;
- an instance made again while the old one's events are still read:
  "live" comes back from the new one's markers alone;
- a watch that fails, or a budget reached, lowers coverage for that
  subtree alone;
- a sync of a live folder answers with a change made just before it;
- a mount made below a watched folder: not covered, a mount.

## Phase 3: Linux, the system service

- fanotify on whole filesystems. On btrfs, use a detached read-only
  top-level mount when marking through a subvolume is refused (DESIGN 3).
- Coverage tests for queue overflow, mount and subvolume changes, and
  handle resolution failures; phase 0 establishes feasibility only.
- The generations and the backoff of phase 2, for marks made again.
  Events dropped as they are read (under private folders) never include
  an overflow.
- Answers filtered per asker (DESIGN 4).
- Each user's registered folders kept per user, each applying only to
  folders that user owns (DESIGN 5).
- The sandboxed unit.
- Tests that need root run in a virtual machine, never on the
  developer's own system.

## Phase 4: Windows

- A user's engine first: the walk and `ReadDirectoryChangesW`.
- Then the service: the file table and the change journal. A position
  kept in the journal is used only if the journal's id is the same, and
  the position is at least its lowest valid USN and not past its next
  (`FSCTL_QUERY_USN_JOURNAL`). Otherwise the check runs (DESIGN 2).
- Tried under Wine as far as Wine answers.

## Phase 5: macOS

- `getattrlistbulk` and FSEvents, from fsearch:
  - events the system dropped, replayed from its log;
  - the position kept never past a gap, nor past a recovery put off
    (fsearch's `052f636`);
  - the position kept used only where the event database is the same
    one, and the position is not ahead of its current id (DESIGN 2).
    macOS 26.3 numbered its events afresh, below fsearch's saved id
    (its `2919777`). The database's identity
    (`FSEventsCopyUUIDForDevice`) is to be verified on a Mac;
  - each stream with its generation: an old stream's events prove
    nothing (fsearch's `61f8c8d`, `d78a4fc`);
  - a latency of 0.5 s with `kFSEventStreamCreateFlagNoDefer`, as
    fsearch 0.2.2 has. A sync flushes the stream, so the latency
    delays no sync;
  - events below folders not covered dropped in the callback. Never
    dropped: those on `/`, where the drop flags arrive, and "history
    done";
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
