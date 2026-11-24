# skwird's plan

Each phase is useful alone, and ends with what was measured and what is
left, honestly. The design is `docs/DESIGN.md`; a phase that changes a
decision changes it there.

## Where it stands (2026-11-24)

Phase 0 is done. Phase 1 is done, its eight steps: a user's engine on
Linux, started by its first client, answering protocol 0, measured on
the user's home and given budgets (DESIGN 10), the three of memory
(after a first build, at its peak, after a sync) met at the second and
third measurements. Phase 2 is done, its seven steps: the engine as a
user service, folders its clients register watched and answered live,
a sync of them a barrier, the mount table followed, within budgets of
its own (DESIGN 10). Phase 3 is done, its eight steps, every budget met
but a sync's, 0.13 s against 0.1, left so by the user's choice (DESIGN
10, "Measured at the end of phase 3"): the system service, fanotify on
whole filesystems, every user answered what they could list (ACLs
evaluated), each user's exclusions kept, a sandboxed unit and a NixOS
module, proven as root in virtual machines. The choices phase 1 waited
on were made on 2026-11-11 (DESIGN 2, 5 and the Protocol): private
folders a client registers, tool stores indexed, network mounts never
crawled, no crate on crates.io. skwir released
0.6.0 on 2026-11-21; its session builds on the client's copies (below,
"skwir, the first client"), and skwird answers what it asked of it: links
taken unfiltered, how far a journal's events are every change, and
folder totals for its Usage view (step E, done on 2026-11-24). Phase 4
has its step 7 done (2026-11-24): a user's
engine on Windows built and tested under Wine; the system service built
and tested on GitHub's Windows runners, which stand in for a Windows
machine, the user having none (below, "Paused, and how it goes on"):
its file table read and checked by, its journal followed and resumed
from, its askers judged by their tokens, its users' exclusions kept, its
volumes followed as they come and go. Left: step 8, the measurement, on
a Windows machine.

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

  Begun on 2026-11-23. skwir's session copied the crates at `40f7f87`,
  read the client, and asked for five changes before building its
  provider on it. Four are made (DESIGN, Protocol, "What
  `skwird-client` owes a client"):
  - every call can be given up on;
  - an engine started runs from `/`;
  - an engine started is reaped;
  - the XDG folders are read as the engine reads them, or named by the
    client.

  The fifth is phase 4's: on Windows, an engine started opens no
  console window.

  The same day, skwir's settings gained a list of the folders kept live
  and a meter of the share, and asked `watching` to say more. It now
  says, for each folder, live or why not, the folders its watches take,
  and what holds it; and the share's use and size (DESIGN's open
  question 2). Three things changed in how the share fills:
  - it fills in the order folders were kept, where it went by path;
  - a folder that does not fit lets go of what it got, where it kept
    it, starving the folders kept after it;
  - a folder left short is tried again once room is let go of, where
    it waited for a restart.

  The test of it found a bug from phase 2. With the share spent, a
  folder watched already, read again on an event, was refused its
  watch, so the folder it was kept for was no longer live. A folder
  watched keeps its watch now, the share spent or not.

  skwir copied the client again at `94a6d65` and built the seam the
  provider plugs into (its ADR-150). It asks an index for name terms and
  kinds alone, never sizes or times yet: in its searches, a link's size
  and times are its target's. When skwir designs that, it may ask
  whether skwird could hold or answer a link's target's attributes.
- **D. The harvest's enumeration and journal** (skwir's ADR-089):
  `changes` from step 4, live from phase 2. A `changes` that a check
  found holds no write made in place: such a write changes no folder's
  time. fsearch's recovery missed files that grew so (its `8e6db08`).
  Live events, from phase 2, hold them.

  Asked by skwir's session on 2026-11-23, for its ADR-150's step D, and
  done the same day: a query may take links unfiltered (`e8c14a6`), and
  `changes` says how far its events are every change (`fd4334d`); the
  fake engine answers changes from its script (`7947e9b`).
- **E. Folder totals** for skwir's Usage view (DESIGN, Protocol,
  `totals`, designed with skwir's session on 2026-11-24). Its steps:
  1. **nlink kept** with each entry: the crawl gives it (statx on Linux,
     the file table's record on NTFS, unknown from Windows' listings),
     and the chunk's layout gains a column for it, so that every index
     is built once again after the upgrade.

     Done on 2026-11-24 (`Attrs::links`, 0 where not known). On NTFS it
     is the file's long names, which the table gathers from all its
     records: the record's own count holds the short DOS names too. The
     chunk's layout is version 2, its column bit-packed as the rest, so
     that a column of ones costs nothing; the log writes a tag with it,
     and reads its older one as none known. Tested: a file of two names
     says 2 by either, on Linux and from the table on a runner, and 0
     from Windows' listings; the chunk and the log read back a count of
     1, 2 and 3.
  2. **The protocol:** `totals`, its batches and its `done`, in
     `skwird-protocol`, the client's call, the fake engine's answer from
     its script, and the transcript.

     Done on 2026-11-24: `Ask::Totals`, `Answer::Children` and
     `Answer::TotalsDone`; `largest` picks the children to send and sums
     the rest, for engine and fake alike; `Client::totals`; the fake
     answers its script's totals, in batches of 512, so many with
     `largest`. Until step 3, the engine answers `unsupported`.
  3. **The engine:** the totals computed from the snapshot, per asker at
     the system service, streamed, the coverage and holes as `query`
     gives them.
  4. **Tested** against a walk of the tree: sizes, allocs, counts, the
     newest time, extensions, a file of two names inside and one of a
     name outside, a hole where a folder is excluded, private or denied.

     Done on 2026-11-24 (`skwird_index::totals`, `skwird::totals`):
     - summed over each child's subtree in pre-order, once a request,
       never kept; a folder not covered, or one the asker may not list,
       a hole whose subtree is passed over;
     - a file of several known names held apart, by its identity, to be
       counted once a subtree; one whose count is not known (Windows'
       listings) counted at each name, the answer partial;
     - the journal's token asked of the writer before the snapshot is
       read, so that the snapshot is at least as new;
     - answered on a thread of its own, as a query, counted among the
       user's queries at once, cancelled likewise;
     - `skwird totals [PATH]`, each child the largest first.

     Dated as Usage dates (skwir's ask the same day): files alone date a
     subtree, each child and the folder asked carry their own times.
     Tested: against a tree of known sums (sizes, allocs, counts, the
     newest file, extensions, a file of two names inside and one with a
     name outside, an excluded folder); through a user's engine (every
     child, the folder's own every child's, the private folder a hole,
     the token, `largest` and the rest); at the system service in its
     machine, as two users: the owner counted their file, the other told
     the home is denied, nothing in it counted.

Kept alike in both: the four coverage words; name matching (DESIGN 7),
so a change to one is a change to the other; and how a network mount
is told (DESIGN 2).

## Phase 2: Linux, a user service (done)

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

   **Done on 2026-11-21.** `skwird_crawl::Watcher` (`linux/watch.rs`).
   - An inotify instance of a generation, non-blocking, its handle given
     for a caller to wait on.
   - It is given the subtrees to watch. Every folder a crawl or a reread
     reads within them is watched through `/proc/self/fd/N` once opened,
     before its entries are read (`Options::watcher`). Each folder read
     says what came of it (`Crawled::watch`, `read_folder_watched`):
     watched by a descriptor, not asked, past the budget, past the
     user's limit, or failed.
   - What it hears is named by descriptor, never mapped to a folder by
     the watcher itself: whoever applies it maps descriptors to folders,
     and reads each folder named again. So an event heard during a crawl,
     before the index has numbered the folder, is kept, not dropped.
   - `Heard`: a folder changed, moved, its watch gone (removed, or its
     filesystem unmounted), or an overflow.
   - Its share of the user's watches: a quarter of `max_user_watches`.
   - Tests pass:
     - a change heard, named by its folder alone; a folder removed, its
       watch gone and let go of;
     - 300 folders written to without pause while the crawl reads and
       watches them: each whose files differ from its listing was heard
       changed. With the watch moved after the listing, the test failed
       in two runs of three;
     - 16,448 files made in a folder unread: an overflow said;
     - an instance made again: the old one still hears, each with its
       generation;
     - a budget of 3 over 5 folders: 3 watched, 2 said past it;
     - only the subtrees given are watched, an excluded folder never.
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

   **Done on 2026-11-21.** `skwird-bench churn FOLDER -- COMMAND`:
   - It watches the folder as the engine is to watch it, and runs the
     command.
   - It reads each folder named again as soon as it is heard, crawling
     and watching its new folders, as the engine would.
   - From the same reads it counts what batches of three sizes would
     have logged.
   - Each change is sized by the log's own encoding
     (`skwird_index::log::size_of`).

   On a full debug build of skwird's workspace into a scratch folder
   (9.7 s, 14,981 events, no overflow, 141 folders watched at most):

   | applied | reads | records | changes | log | 16 MB last |
   |---|---|---|---|---|---|
   | at once | 7,462 | 6,972 | 18,410 | 3.0 MB | 55 s |
   | 100 ms quiet, 1 s at most | 269 | 227 | 5,928 | 688 KB | 4 min |
   | 1 s, 10 s | 132 | 124 | 2,837 | 272 KB | 10 min |
   | 5 s, 60 s (fsearch's) | 132 | 124 | 2,837 | 272 KB | 10 min |

   A release build of the engine after it (18 s, 2,534 events) logged
   438 KB at once, 87 KB at 100 ms and 1 s, 39 KB at 5 s and 60 s.

   Decided (DESIGN 8): every watched folder gathers its events for 100
   ms of quiet, or 1 s at most, then is read again once. A sync applies
   what is gathered at once. No class of busy folders, and so no set of
   them kept.
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

   **Done on 2026-11-21.**
   - **The index** (`skwird_index::update`). The refresh is cut in its
     parts, so that an install shares them:
     - `install` watches every folder of a subtree and compares it
       through its handle (`skwird_crawl::check_watched`): opened, watched,
       then its identity and change time read. A change before the watch
       moves the change time; one after is heard.
     - `reread_folders` reads again the folders heard.
     - Each says what came of every folder's watch (`Refreshed::watches`),
       `graft` gives the numbers it gave, and the packer keeps a first
       build's watches.
   - **The keeper** maps descriptors to folders both ways, and lets go of
     the watch of a folder no longer in the index, or numbered again.
     - `set_scopes` installs each new subtree, live from when its install
       began if every folder in it was watched. A folder past the budget,
       past the user's limit or whose watch failed keeps it checked, and
       says why (`Live::short`).
     - `heard` applies what was heard.
     - `overflowed` ends every "live" at once; the next refresh brings it
       back, the watches having stayed.
     - `lost` does the same for a subtree whose own folder moved or went,
       which no watch above it follows.
     - The live subtrees travel in each snapshot (`Index::live_since`), and
       coverage within one says live for names and attributes alike.
   - **The engine.**
     - One watcher for every root, listened to on a thread that waits on
       it alone.
     - The writer gathers what it hears a folder at a time (`live.rs`,
       100 ms of quiet or 1 s at most), and waits with a deadline only
       while something is due: a gathering, the check after an overflow,
       a save 10 s after the first change unsaved.
     - After an overflow, the check waits for 2 s of quiet, doubling to
       60 s while overflows follow within 300 s of a check. A sync applies
       what is gathered for its root at once, then checks.
     - `watch {add, remove}` keeps a client's folders live, kept across
       restarts (`$XDG_STATE_HOME/skwird/watched`), as wanted before the
       first crawl, so that a first build watches as it reads.
     - `skwird watch [--remove] FOLDER...`; `SKWIRD_WATCHES` sets the
       budget, for tests.
   - Tests pass:
     - the watcher's checks of step 1;
     - the keeper's, through the real watcher (`tests/live.rs`):
       - an installed subtree is live, and only it;
       - a folder made, filled and moved within it is applied, its new
         folders watched before they are read;
       - a folder moved out and back leaves nothing behind;
       - an overflow ends "live", and the refresh after it reads what was
         dropped and brings "live" back;
       - a budget reached keeps a subtree from live;
       - a folder removed lets go of its watch;
     - the engine's, a process of its own:
       - a watched folder answered live without a sync, within a second;
       - a folder made, filled and moved, answered live;
       - an engine stopped while 16,448 files were made in a watched
         folder: its coverage lowered once it read the overflow, then live
         again after the check, every file answered;
       - past the budget, checked, not live;
       - registrations kept across a restart; taken back, checked.
   - Not done here: the sync of a live folder still runs the check of its
     whole root (step 4 makes it a barrier). A watcher made again (a
     second generation) does not happen yet: there is one a run.
4. **The sync barrier** (open question 1). A cookie made in a folder of
   the engine's own, watched by the same instance, so in the same queue:
   heard, every event before it has been read. A sync of a live folder
   answers once the cookie is heard and what came before applied: live,
   up to a time after the sync was asked. Elsewhere, the check, as in
   phase 1.

   **Done on 2026-11-21.**
   - The watcher watches a folder of the engine's own for cookies
     (`watch_cookies`, outside any subtree and the budget). A file named
     `cookie-N` made there is heard as `Heard::Cookie(N)`, in its turn in
     the queue.
   - The engine makes that folder in its runtime folder
     (`$XDG_RUNTIME_DIR/skwird/cookies`, its user's alone), and lets go
     of cookies a dead engine left.
   - A sync whose every path asked is live makes a cookie, and waits for
     it without blocking the writer: the cookie comes through the same
     channel as every event. Heard, the root's gatherings are applied
     and the sync answered. Only the current watcher's cookie counts.
   - A cookie not heard within 2 s (its event dropped, its folder gone)
     falls back to the check, as does a sync of a folder not live.
   - Tests pass:
     - five syncs, each just after a file is made in a live folder: each
       answers live, the file in its answer. With the gatherings not
       applied on a cookie, it failed at once;
     - a sync of a live folder checks nothing else: a file made just
       before in a folder nothing watches is not answered, and is after
       a sync of the home, which checks.
5. **The mount table followed,** as skwir follows it (`POLLPRI` on
   `/proc/self/mountinfo`): a mount made or taken below a root lowers
   coverage at once, for its subtree.

   **Done on 2026-11-21.**
   - The engine's listening thread waits on `/proc/self/mountinfo` for
     `POLLPRI` beside the watcher, and reads it through after each
     change, so that it signals the next. A change sends the writer
     `Command::Mounts`.
   - Each keeper reads its table again (`Keeper::mounts_changed`), and
     reads again the folder at each mount point made or taken below its
     root. A mount made is a hole at once: its names are the mount's,
     never the folder's. A mount taken lets the folder be read and
     crawled.
   - Without a watcher, nothing is followed: each check reads the table
     first, as in phase 1.
   - Test, in a namespace of its own: a tmpfs mounted over a folder
     below a watched one is a hole ("a mount") at once, its names gone,
     the rest still live; unmounted, it is read again. With the writer
     ignoring changes to the table, it failed.
6. **The user unit.** `dist/systemd/skwird.service` and `skwird.socket`:
   the engine for the session, idle never ending it. Socket activation
   (`LISTEN_FDS`) where a package installs the socket. Tested with
   `systemd-socket-activate`; no test installs a unit on the developer's
   own system.

   **Done on 2026-11-21.**
   - `dist/systemd/skwird.service`: `skwird serve --forever`, restarted
     on failure, Unix sockets alone (`RestrictAddressFamilies=AF_UNIX`,
     rule 6), no new privileges, a crawl yielding to the user (`Nice=10`,
     idle I/O).
   - `skwird.socket`: `%t/skwird/socket`, mode 0600 in a folder of mode
     0700, where clients look. `systemd-analyze verify` accepts both, but
     for `ExecStart`'s path, which a package or the user fills in.
   - The engine answers on a socket a service manager passes
     (`activation.rs`: `LISTEN_PID`, `LISTEN_FDS`), checked to be a Unix
     stream socket listening, taken once. It binds none of its own then,
     and leaves the socket in place when it goes.
   - README: `skwird watch`, and how to install the unit.
   - Tests pass:
     - the units run the engine for the session on the socket clients
       look for;
     - started by `systemd-socket-activate` at the first connection, the
       engine answers on the socket passed, the same inode before and
       after, and leaves it in place when it goes. With the passed socket
       ignored, the first connection was never answered.
7. **Measured,** by the user, on their home: `tools/measure.sh` gains a
   live step (memory and processor time with the watches of a large
   folder, idle with them, the time from a change to its answer).
   Budgets added to DESIGN 10.

   **Ready on 2026-11-21, waiting for the user.** `tools/measure.sh
   --only live` (in every full run too):
   - **The time from a change to its answer:** a file made twenty times in
     a probe folder of the measurement's own cache, which the engine
     watches, each time until `skwird find` answers with it.
   - **The cost of the folder measured, watched whole** as far as the
     engine's share goes, the most watches a user's engine holds: the
     time it takes, the watches held (the kernel's memory, about 1 KB
     each), the memory then, a sync of it, and 30 idle seconds once past
     the save that follows what was heard.

   Its trials, on a synthetic tree of 9,650 folders, found two bugs,
   both fixed with tests that fail without the fix:
   - **A folder made after the index last read its parent, then
     registered,** was installed with nothing in it to watch, and claimed
     live. Now the nearest folder above it held is read first, so that it
     is crawled and watched; one that does not exist is not live, and
     each refresh tries it again.
   - **The engine's own folders, within the folder it watched:** it heard
     its own saves and log, and read itself again without end, a tenth of
     a core idle. They are now never indexed (DESIGN 5).

   Then, on that tree:
   - a change answered in 113 ms at the median, 121 ms at most;
   - 9,649 folders watched in 0.12 s, the engine at 19 MB;
   - a sync of them, live, in 0.02 s;
   - 30 s idle with them: no processor time.

   **The user's first run** (at `1439be6`, 6.9 million entries in 867,327
   folders):
   - a change answered in 158 ms at the median, 179 ms at most;
   - the home watched as far as the share goes, 131,072 folders, in
     5.6 s, the engine at 276 MB;
   - a sync of it, checked: the home needs more than the share;
   - 30 s with no client asking: **2.27 s of processor time**, 7.5% of a
     core.

   Taken folders in name order, the share watched the dot folders first:
   caches and profiles that applications write to without pause. A trial
   of what a read costs, on a synthetic tree, showed why it cost so much.
   A folder of 100 entries read again cost 3.6 ms, changed or not, since
   a change rewrites its whole chunk of 64k entries. Fixed (DESIGN 8):
   - a folder read back as it was is not written again (97 µs);
   - chunks of 16k entries (a read that changes something, 1.1 ms).

   The engine also says, at each save, what it did for what it heard
   since the last one: events, folders read again, entries stat-ed, the
   largest folder and the busiest's count. These are counts alone, so a
   measurement shows where the time goes. `--only` takes several steps
   (`engine,live`), since the chunks' size touches the load the engine
   step measures.

   Left: the user's second run, `tools/measure.sh --no-root --only
   engine,live`, then the budgets.

   **The user's second run** (at `ba3ed1e`):
   - The engine step, with chunks of 16k entries: read back in 0.63 s at
     181 MB, a warm sync in 0.62 s, no processor time idle, 288 MB on
     disk. The chunks' size cost nothing measurable.
   - Live: a change answered in 143 ms at the median, 161 ms at most;
     the share of 131,072 folders watched in 4.3 s; the engine at
     255 MB.
   - Still 2.82 s of processor time in 30 s with no client asking. The
     counts at each save said why: few folders read again (10 to 23 each
     10 s, 1 to 3 distinct), but one of 62,000 entries heard about once a
     second, each read stat-ing all of them (186,000 to 776,000 entries
     stat-ed each 10 s).

   Fixed (DESIGN 8): a folder read again stats only the entries its
   events named, and those new to its listing. Its names are still every
   name the listing holds, and every other entry keeps what it was read
   with, if the listing gives it the same inode and kind. The watcher says
   each event's name (`Heard::Entry`), and the gathering keeps up to
   4,096 names a folder. A test: a folder of 2,000 entries, one file
   written to, read again with at most two stats, its new size in the
   index.

   **The user's third run** (at `2e7ad76`): the entries stat-ed fell to
   2 to 21 each 10 s (from up to 776,000), but the engine still took
   1.64 s of processor time in 30 s. Stages timed on a synthetic folder
   of 62,000 entries showed a read costing about 40 ms without a single
   stat: the listing, what was kept, the merge, each a microsecond or
   less an entry. A read of such a folder costs its entries, and only
   trusting events for names would change that. The listing keeps them.

   So (DESIGN 8) applying what is heard keeps to a share of a core:
   - after each application, each folder read waits for its share of the
     processor time taken, by its entries, 49 times over: at most 2% of
     a core;
   - the time counted is the writer thread's own, not the clock's. With
     the clock's, a busy disk's fsync held a probe folder for 10 s;
   - each folder waits for itself alone: a small one read beside a large
     one does not wait for it.

   On a synthetic tree, a folder of 62,000 entries written every 100 ms
   cost 1.4% of a core; a change in a small folder meanwhile was
   answered in 122 to 282 ms; the probe's changes in 115 ms at the
   median; a sync of a live folder, a change made just before, 6 ms.
   The live step now times that sync too.

   A hasher faster than the standard one was tried for what is kept of
   a folder, and changed nothing: removed.

   **Done on 2026-11-22.** The user's fourth run (at `7d2232f`, on a
   machine at a load of 20 to 35, their home now 6.7 million entries in
   798,530 folders): 0.53 s of processor time in 30 s watching the
   share, 1.8% of a core, under the duty cycle's 2%. The other figures
   were the load's, 2 to 7 times the quiet runs': the build 136 s, the
   install 24 s, a change answered in 411 ms at the median, a sync of a
   live folder in 54 ms. The budgets (DESIGN 10) take latency, install
   and memory from the two quiet runs before, which the duty cycle does
   not change (it holds only folders whose reads are costly), and
   processor time from this one: all met.

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

## Phase 3: Linux, the system service (done)

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

### Steps

1. **The machine** (done on 2026-11-22). The flake's `checks` are the
   virtual machines, made by NixOS's test framework
   (`pkgs.testers.runNixOSTest`, QEMU): a machine declared as a NixOS
   system, with its users, its units and its disks, the kernel the
   flake's nixpkgs pins, and the host's store shared. Neither a
   container, which would mark the host's own filesystems, nor a
   one-command machine (virtme-ng, vmtest), which sets up no users or
   services. The flake also gained what such a machine installs, made
   as skwir's are: the package (`dist/nix/package.nix`, in nixpkgs'
   manner, built with the pinned Rust alone) and a NixOS module
   (`dist/nix/module.nix`, `services.skwird`: skwird installed, and
   with `session`, on by default, the user service for every user).

   The first machine, `user-service` (`tools/vm/user-service.nix`),
   proves phase 2's service on a system of its own, with two synthetic
   users. Every client in it sets `SKWIRD_NO_START`, so what answers is
   the service's or nothing:
   - the socket starts no engine before a client;
   - the first client starts the service's engine, which answers what
     the user made (a name with a space, one with an accent), and it is
     the only engine;
   - a folder watched is answered live, with no sync;
   - restarted, the engine watches what was registered again;
   - each user's engine is their own, and the other user cannot reach
     their socket.

   It passed on the developer's machine with its virtualization off,
   the processor emulated: 187 s to boot, 228 s in all; with KVM, 10 s
   to boot, 17 s in all.
2. **Root tests in a machine.** Tests that need root are built by cargo
   on the host and run as root in a machine, the host's build folder
   shared into it, with disks of its own (ext4, btrfs with subvolumes);
   a pure check builds them with Nix for `nix flake check`. They skip
   anywhere else, `sudo cargo test` on a developer's system included.

   **Done on 2026-11-22.** A test that needs root is in a
   `tests/root*.rs` of its crate and asks `skwird_testkit::machine()`
   first: it runs only as root in the machine, where the machine names
   its filesystems in `/etc/skwird-test-machine`, and is skipped
   anywhere else. The machine sets `SKWIRD_TEST_MACHINE`, so that a test
   that would skip there fails instead. `tools/vm/root.nix` is the
   machine: two disks of its own, one ext4, one btrfs mounted through a
   subvolume (`home`) holding two more (`a`, `b`); Linux 6.18.58, as
   nixpkgs pins it. `tools/vm/root-tests` builds the tests with cargo,
   shares them into the machine and runs them: 6 s to boot, 14 s in
   all. `checks.root` runs the same machine with tests Nix built. The
   gate runs the root tests too (`--no-machine` skips them).

   The first test proves the machine is what the tests rely on: each
   filesystem's kind, each subvolume a device of its own, its top folder
   inode 256 (DESIGN 6).

   Clippy 1.99, which the gate runs since the toolchain moved, asks for
   `assert_eq!(x, Vec::new())` over `assert!(x.is_empty())`, which shows
   the value on failure: four tests changed.
3. **fanotify** in the crawl crate, from phase 0's probe: one mark per
   filesystem, btrfs through the detached top-level mount, events
   resolved to their folders by handle; an overflow, a mark refused and
   a handle that does not open lower coverage, for what they concern.

   **Done on 2026-11-22.** `skwird_crawl::fanotify`: a group, its marks,
   its events read as folders by handle and names, and a resolver that
   opens a handle to the folder's device and inode (DESIGN 3, "As
   built"). Ten root tests prove, in the machine, on ext4 and on btrfs:
   - a change heard by its folder's handle, which opens to it: names
     with a space and an accent, made, moved and removed, and the
     folders made, each in its parent;
   - btrfs marked through its top level once for all its subvolumes,
     the mount made for it gone, and heard in each;
   - an overflow;
   - a folder removed: its handle gone, its removal heard in its parent
     after;
   - a folder moved, heard in both parents; a folder's mode changed,
     heard as the folder itself;
   - a cookie after a change on another filesystem: one queue (open
     question 1);
   - a mount over the filesystem's folder not taken for it;
   - a filesystem without handles (ramfs) refused.

   Root tests run one at a time in the machine: they mark and mount on
   the machine's shared filesystems. Not done here: the engine that
   uses it (step 4), and a test of two filesystems with one id, which
   needs two filesystems made alike.
4. **The system engine** (`skwird serve --system`): its roots (DESIGN
   1), its socket (`/run/skwird/socket`), its index readable by it
   alone, every folder's owner, group, mode and ACL, live from fanotify
   with phase 2's machinery (generations, gathering, the duty cycle,
   the barrier).

   **Done on 2026-11-22.** `skwird serve --system` (DESIGN 1, "As
   built"): its roots from the mount table, its folders its service
   manager's, its socket open to every user, and every root's
   filesystem marked before it is read. A root is then heard whole
   (`Keeper::heard_whole`): live from the mark, once its crawl, or the
   check after loading, has proven it current; checked, never live,
   where its filesystem refused the mark. What the group hears is found
   by handle, at its path, in the root holding it, known by its
   identity, and gathered as phase 2 gathers (the duty cycle, the
   barrier: the cookies in its state folder, on a marked filesystem).
   An overflow, or a handle that fails to open, lowers every root it
   concerns until the check after; a filesystem no longer mounted is
   not live again until the service starts again.

   Found on the way, and fixed for both engines: a root kept its folder
   open as long as its index lived, so the system service would have
   made every root's `umount` fail. A root's folder is now held only
   while it is read (AGENTS.md, "Hold nothing of a filesystem between
   reads").

   Root tests in the machine: the service marks ext4 and a btrfs
   subvolume (through its top level) and answers both live, with what
   was there, a change made with no sync, one made just before a sync,
   and a folder removed; it refuses a user other than root. A root is
   unmounted between two reads; once its mount is gone, it is not
   covered. A folder in a subvolume no mount reaches is unreachable.

   Until step 5 the service answers root alone (rule 2). Not done
   here: the folders' ACLs (step 5); each user's own files (step 6),
   so a user's own exclusions hold only once step 6 is done; a
   filesystem mounted after the service started, a hole until it starts
   again (done after, on 2026-11-22: a root from then on, and one
   unmounted let go of, proven in the system-service machine); a query
   above several roots, answered from one (done after, on 2026-11-22:
   each answers, at its place). Snapshots (DESIGN, open question 10),
   answered by the user on 2026-11-22: below a root, left out.
5. **Answers filtered per asker** (DESIGN 4): `SO_PEERCRED` and
   `SO_PEERGROUPS`, modes and ACLs evaluated, each folder's answer kept
   per asker until it changes, limits per asker.

   **Done on 2026-11-22**, but for the ACLs' evaluation (DESIGN 4, "As
   built"): the asker from the socket, modes evaluated as the kernel
   takes them, a result given if its folder may be read and searched
   and every folder above it searched, judged inside the query's scan
   so that limits hold and no folder hidden is walked. A folder with an
   ACL is not known: what it holds is withheld, and it is named not
   covered. A folder the asker may not list, or a path below one, is
   answered denied, whether it exists or not. Root tests: three users
   (one in a folder's group, one not, root) asking one tree with a
   folder only its owner reads, one its group reads, one any reads, one
   that may be passed through but not listed, and one with an ACL; each
   is answered exactly what they could list, and the ACL's folder is a
   hole for both users. Not done here: the ACLs evaluated (step 5b,
   below), the answers kept per asker, the limits per asker; the journal
   and registrations, refused to other users until step 6.

   **5b, the ACLs evaluated, done on 2026-11-22** (DESIGN 4, "As built"):
   `skwird_crawl::acl` reads an access ACL as the kernel writes it and
   judges an asker by it as acl(5) does, tested on its own (a named user,
   a named group, the mask, a matching group never falling to the
   others', and the bytes Linux 6.18 writes). The keeper reads each
   flagged folder's ACL through the root, keeps it by identity until its
   change time moves, and puts the ACLs in each snapshot. In the root
   test, the folder whose ACL names one user is answered to that user,
   hidden from the other, with no hole; an ACL granted to the other while
   the service runs is heard and answered at once. Not done: a folder
   with an ACL above a root, and one deep in a subtree just made, which
   stay not known until the next check.
6. **Each user's exclusions** (DESIGN 5): the default ones in every
   home, each user's files read, registrations kept per user, each
   applying to folders that user owns.

   **Done on 2026-11-22** (DESIGN 5, "As built"). A root test adds two
   users to the machine's user database, their homes in the test's
   place, and starts the service: one user's file and their engine's
   registrations keep their folders out, read before any client of
   theirs connects; a folder their file names that another user owns
   stays; the other user's file, which others may write, is ignored;
   what the first registers with the service holds at once, and their
   registering the other's folder does nothing. Registrations at the
   service are kept with their user's id. The journal is still refused
   to users other than root; a `watch` at the service is answered with
   the folders asked, every folder being heard already.
7. **The sandboxed unit,** the module's `services.skwird.system`, and a
   machine proving the service whole with two users.

   **Done on 2026-11-22.** The unit (DESIGN 4, "As built"), its user
   for systemd-sysusers, both in the package; the module's
   `services.skwird.system`, which turns the user's engine (`session`)
   off by default, since every client finds the system service first and
   nothing that watches runs twice for one user (rule 4). The machine
   `system-service` (`tools/vm/system-service.nix`) proves it whole, with
   two users, one in a group: the service runs as its own user with two
   capabilities alone, its index closed to users; every client finds it;
   a change is heard live and answered to who may list it (one user's
   home 0700, the other answered nothing there and told it is denied);
   a folder its group reads is answered to the group alone; `~/.ssh` is
   never indexed, and a line the user adds to their file holds from their
   next sync.

   Found on the way: `DynamicUser` implies `RestrictSUIDSGID`, which
   makes systemd refuse `openat2` whole, so the service started, found no
   root it could read, and said the kernel lacked `openat2` (now: "or
   lets a system call filter refuse it"). A run of the machine tried each
   option alone to find it. The folders' check now follows a link to the
   folder it names, where systemd makes them links. And a line added to a
   user's file held from a checked sync alone, not from a sync of a live
   folder, which the barrier serves: in both engines, since phase 2. It
   holds from every sync now, and a test of the user's engine proves it
   (it fails without the fix).
8. **Budgets,** measured by the user on their machine, as root (done on
   2026-11-23: every budget met but a sync's, 0.13 s against 0.1, left
   so by the user's choice; a storm's cost since the reader's longer
   wait measured in the root tests' machine alone).

   **Ready, not run:** `tools/measure.sh --only system` (README,
   "Measuring"), never run unless named. It runs the system service as
   root over the whole machine, its folders in the measurement's cache
   and its socket where a user's client looks for its own, so that it
   meets no engine of the user's; it refuses where a system service
   already answers. Asked as the user, it times the first live answer, a
   change answered, a sync of the home, a query of the home filtered for
   the user and the same as root, idle processor time and memory, then
   the first live answer read back. It could not be tried here: it needs
   sudo, which a session has not.

   **Run on 2026-11-22** by the user, at `50c540d`, on their machine: it
   ran as written. The budgets are set (DESIGN 10, "Budgets for the
   system service"): six met, a change answered in 231 ms at the median,
   the first live answer read back and checked in 1.35 s for 6.95
   million entries, a user's query 1.17 times root's. Not met: 2.8% of a
   core idle, over the 2% the duty cycle holds applying to, since finding
   each event's folder and saving are outside it. Not set: the first
   build, near cold in this run (28.9 s), three times phase 1's warm
   build of the home; the ACL flag's call, timed on a synthetic tree,
   costs a few percent, not that. A warm run sets it. At its first sync,
   one root held to its exclusions afresh (one folder purged): what the
   rule of deriving them again is for (DESIGN 5).

   **Reviewed on 2026-11-22**, by a reader not anchored on the work, for
   what a user other than root could learn or do. Fixed, each with a
   test:
   - holes were given to every asker, naming other users' private and
     excluded folders: a hole is now said only to who may list its
     parent (DESIGN 4);
   - the default exclusions followed a user's links as the service: a
     link at `~/.gnupg` took another user's folder out for everyone;
   - a user's files were ignored, without a word, where umask 002 left
     them group-writable for a group of their own;
   - a refused registration was kept and said kept; one connection's
     finished queries held their threads' stacks until it closed;
     connections at once had no limit (64 now);
   - a folder whose ACL was read was still a hole, and a hole for one
     not read came only when a name matched.

   The writer's load each user can ask for is held to once a second a
   user (a sync, a registration, the journal). Left, as the review found
   them: what one such ask costs
   (a sync derives every exclusion again and may check a whole
   root, registering and withdrawing a folder refreshes its root,
   reading the journal reads the log) and no time limit on a query. The
   folders above a root, taken as they were at the start, are judged
   live since (a root test closes one while the service runs).

   **The budget not met, 2026-11-22.** The save's log line now gives the
   writer's processor time by kind: finding, applying, saving, giving
   memory back. A root test reproduced the user's machine: a folder of
   59,000 entries written every 100 ms, with the service watching. In
   the debug build, about 40 ms went to finding and 220 ms to applying
   in each 10 s, and saving under 5 ms; one read of the folder stat-ed
   one entry and cost about 200 ms, in four passes over all of it. Saves
   were not the cost, as had been thought. Since (DESIGN 8):
   - the share is 1%, half the budget, and counts finding and saving
     with applying;
   - the folders it limits divide it;
   - a folder found is kept as found while its index keeps its shape.
     Kept by its identity alone, a folder moved lost what was heard in
     it: the root test written for it failed so, and passes with the
     shape.
   Built optimised in the machine, the trial took 2.3% of a core before
   and 1.3 to 1.6% after, a small folder's change answered in 112 to
   119 ms throughout. On the user's machine, to be measured again
   (`tools/measure.sh --only engine,system`, which sets the first
   build's budget too).

   **Measured again on 2026-11-22**, at `8cb2d43`, at a load of 6 to 15:
   most figures 3 to 5 times slower than before, those of untouched code
   (a user's engine reading its index back: 3.48 s against 0.63 s)
   alike, so the run sets no budget. Idle, 4.4% of a core. The save's
   log line showed why, whatever the load: 20 to 27 folders of up to
   15,756 entries read again in each 10 s, each at its own pace, which
   no share bounded. The user chose the budget over the lag: the writer
   now keeps to its share whatever the folders do (DESIGN 8: an account
   for what no folder's own hold bounds, batches that fit it, the least
   used first, the reader batching what it hears). Its threads are
   named (`skwird-writer`, `skwird-heard`, `skwird-client`,
   `skwird-query`). A trial of 30 folders of 3,000 entries written
   every 100 ms took 20% of a core before, 0.9% after. A quiet folder's
   change waited 2.4 s, one changing again and again about 55 s: what a
   read costs (a whole folder and its chunk for one file) is what lags
   now, the next thing to make cheaper. To be measured again on a quiet
   machine.

   **On a quiet machine**, at `2d5571b` (load 0.6 to 3.8): the first
   build's budget is set (3 s per million entries; 2.26 measured);
   read-back, queries, memory and processor time idle (0.6%) met. Not
   met, the change answered (1.39 s at the median, 10.0 s at most) and
   the sync (0.19 s): the account, at 0.5%, was below what the writer's
   work costs on a quiet desktop. Now one account at 1.5% for all of it,
   large folders included, each of the writer's turns charged whole
   (DESIGN 8): in the trial, the writer at 1.5%, a quiet folder's change
   in 309 to 399 ms. To be measured again. Found meanwhile: a user's
   engine's warm sync takes 1.16 s where phase 1 measured 0.43 s, its
   cause not found yet.

   **A cheaper read** (the user's go-ahead, 2026-11-22). A trial of the
   index crate (`tests/reread_cost.rs`, ignored, an index of 900,000
   entries) showed a folder of 8 entries read again for one file at
   2.4 ms, its chunk decoded and encoded whole. A block of the same shape
   (a file written to) is now patched into its chunk, and a block decoded
   once a read, a run at a time (DESIGN 8): 60 µs for 8 entries, 1.2 ms
   for 3,000, 30 ms for 60,000, against 2.4, 3.4 and 42. Measured on a
   machine at a load of 7 to 19, whose own work made the root tests'
   machine's trials say nothing this time; to be measured again, quiet.

   **Measured again**, at `5f9997b` (DESIGN 10): a change answered in
   200 ms at the median, 629 ms at most, met again; idle 1.1%; the first
   build 1.37 s per million. Not met, the sync (0.12 s) and memory at
   rest (26.8 MB per million once built, 32 after the queries). Since: a
   sync's cookie no longer waits for the readers' batching, the
   measurement says memory in two (the process's own, its files mapped),
   and the save's line what a read costs. To be measured again, quiet.

   **Measured at `e7417bb`**, at a load of 6 to 24: memory said in two
   for the first time. Most of an engine's is its index's files mapped
   (24 MB per million entries at both); the system service's own was
   8.0 to 8.7 MB per million, a user's engine's 1.6 to 2.9. The user
   chose to budget the two apart (DESIGN 10): its own 10 MB per million,
   its files mapped no more than its index on disk. Both met. glibc's
   arenas, limited in the root tests' machine, did not explain the
   service's more, and are left. A read cost 0.9 to 1.7 ms at the median
   there: the save's line now says how each read was written (as it was,
   patched, spliced, cut again), and a file made or removed is spliced
   into its chunk's columns, not cut again (DESIGN 8): 464 µs for a folder
   of 8 entries, against 1,224. To be measured again, quiet.

   **A spinning disk mounted in the home** (the user's run at `a85cfa3`):
   the system step failed, no root live after 30 minutes, the disk's
   crawl holding every root, since the writer opened every root itself,
   in turn, before listening or answering. Each root now opens on a
   thread of its own, what is heard for it meanwhile held (DESIGN 1, "Each
   root is opened on its own"); a root test on a slow device (dm-delay in
   the root tests' machine) proves it. The measurement now runs both
   engines as their units do (niceness 10, idle I/O), prints the
   service's log when a step fails, and says when the home is answered
   live and when every filesystem is built.

   **With the disk mounted again** (the user's run at `52e2932`, at a
   load of 31 to 45): the home was answered live in 21 s, the disk still
   crawled, against more than 30 minutes before. Found:
   - "every filesystem built" counted lines, and a filesystem mounted
     during the run gave the last: the count is now by root number, and
     the steps after it measured the disk's crawl still going (11.2 s of
     processor time in 30 s, changes answered in 10 s);
   - read back, the home's index was built again, the log not saying
     why: it says now. Not reproduced: the keeper's kill test loaded 60
     of 60, and a machine probe with mounts changing under a root and
     the service killed loaded every time;
   - under that load, the writer's account was overdrawn (up to 38,587
     events in a save's 10 s, finding up to 1 s of them, a folder of
     89,740 entries read whole in 1.8 s), so changes waited the window.

   **Measured quiet**, at `00067fa`, the disk unmounted (DESIGN 10): every
   filesystem built in 7.4 s, the roots at once; read back, the home's
   index loaded; met but for a change answered (279 ms at the median,
   against 250) and the sync (0.13 s, against 0.1), which seem to share
   the reads of the user's busy folder of 65,868 entries, 52 to 63 ms
   each. Next: a large folder's read made cheaper.
   Done (DESIGN 8, "A large folder's read costs its listing"): a listing
   read in place and sorted, what is reused found by walking, the merge
   taking the listing as it is, a changed chunk renumbered at the save.
   A folder of 60,000 entries read again in 18.5 ms against 26 to 31.
   To be measured again, quiet.

   **Measured at `0376f33`**, at a load of 3.5 to 7.6, the caches cold: a
   change answered in 219 ms at the median, met; the sync 0.18 s, not.
   Reads of small folders still cost 1.5 to 2.3 ms at the median. Read
   through a keeper, as the service reads, a folder of 1,000 folders took
   1.3 ms: views opening every column for one value, and the ACLs looked
   at for every subfolder and their map made again at each application.
   Now 646 µs (DESIGN 8, "An application costs what it changed"). To be
   measured again, quiet. Seen once in the gate: inotify's test of a
   folder removed found its watch gone in a second read, under load, a
   test sensitive to timing.

   **Measured at `520c664`**, at a load of 2.3 to 5.4: every one of the
   ten changes waited past the measurement's 10 s, the account owed for
   good. Builds took 23 to 24 s: across this day's runs, they took 7 to
   92 s by the machine's load, 7 s alone at a load of 0.5. By the save
   lines, what owed the account was finding (371 ms for 3,345 events
   that made 2 folders read, both as they were; 655 ms for 272), and a
   few large reads (323 ms for two folders, the larger of 8,192
   entries). Each event in a folder no index holds (the service's own,
   the Nix store, outside every root) was found again at each batch. Now such a folder is kept as found in none while no index
   changes its shape (DESIGN 8, "What no index holds is found once").
   In the root tests' machine, a storm outside the root costs a sixth of
   what finding it took. To be measured again.

   **Measured at `39f5d2e`**, at a load of 8.6 to 14.6: a change
   answered in 143 ms at the median, one of ten not within 10 s; the
   sync 2.27 s; the query as the user 2.05 s against 1.06 as root. Of
   the folders events named, most were found as before; those in no
   index were taken as known so (1,428 of 1,440 in one interval). The
   sync's applications had been counted as finding and charged: now
   served apart (DESIGN 8, "A sync is served apart from what is heard").
   A test holds that a folder's ACL stays answered through applications
   and saves and follows its own change. Not known: why the query as
   the user grew against root's, 1.09 to 1.43 to 1.93 times over three
   runs; and how much of the sync's time is the disk's, the service's
   I/O idle (`IOSchedulingClass=idle`) under the load.
   Found under that load: the crawl listed a watched folder from the
   folder opened before its watch, which btrfs lists without what was made
   since; what was made between was missed by the user's engine. Now
   listed through the folder opened again after its watch (DESIGN 8, "A
   folder watched is listed through a folder opened after its watch").
   The two tests of the watches that failed under load are made to hold
   there.

   **Measured at `f19f5ea`**, at a load of 6.7 to 13.7: a change
   answered in 264 ms at the median and 9.67 s at most; the sync 0.13 s;
   the query as the user 1.09 s against 1.17 as root (0.93 times: its
   growth over the three runs before was the load's); read back 1.12 s;
   idle 1.3%. A filesystem unmounted while it ran was let go of. The I/O
   class cannot be what held the sync: the home's disk takes no I/O
   scheduler, which leaves the classes unread. The measurement now says,
   for the sync and the queries, what the service's threads did
   meanwhile (on a processor, waiting for one) and the client's time:
   whether the service's niceness holds them.

   **Measured at `c45e4aa`**, at a load of 4.9 to 11: the sync 0.20 s,
   its service threads 0.12 s on a processor and none waiting for one;
   the queries 0.70 and 0.92 s, none waiting either. The service's
   niceness holds neither: step 2 of the recommendation is not taken.
   The client's time read 0.00 s, wrongly: `time` wrote its decimals
   with the locale's comma; now read either way. A change answered in
   3.83 s at the median, two of ten not within 10 s; idle 2.5% (not
   met). While the home was built, finding cost 273 ms for 1,939
   events: folders in a root being opened were found again at each
   batch. Now kept (DESIGN 8, "What finding gave is kept, whatever it
   gave"). The account is still owed on this desktop: saves of 67 to
   116 ms and reads of 75 ms of the large folders are charged to it.

   **Measured at `c4c55ca`**, at a load of 10 to 22, the heaviest of the
   day: a change answered in 3.0 s at the median, four of ten not within
   10 s; the sync 0.53 s (0.41 s on a processor, none waiting for one);
   idle 2.4%. While the home was built, finding took 1 to 8 ms a save
   interval against 273 ms. Saves cost 18 to 86 ms each, every root
   saved whether changed or not. The user's choice: keep the budget,
   make the saves cheaper. Now a root with nothing new is not saved,
   and a save reads its names again only where it wrote one (DESIGN 8,
   "A save writes what is new"). Still to see: what each chunk made
   durable costs a save of many folders.

   **Measured at `000d4d3`**, at a load of 4.6 to 10: a change answered
   in 211 ms at the median and 361 ms at most, all ten; the sync 0.10 s;
   the query as the user 1.11 times root's; read back 1.12 s; saves of
   1 to 25 ms against 18 to 86. Every budget met but idle: 0.66 s in
   30 s, 2.2% against 2%. The measurement now says the idle time by the
   service's threads: whether the writer keeps to its account (1.5%) and
   what reading the events takes beside it.

   **Measured at `cbb07a7`**, at a load of 5.5 to 17.5: none of the ten
   changes answered within 10 s; the sync 1.13 s, 0.97 s of it on a
   processor (146 folders waiting, taken at once); idle 0.57 s in 30 s,
   the writer 0.39 s and the reader 0.19 s. Half the writer's time was
   its turns' bookkeeping, about 160 µs a wake, going over every folder
   waiting, which the owed account made many. Now bounds keep a turn
   at 63 to 86 ns whatever waits (DESIGN 8, "A turn costs what it does,
   not what waits"). Still to see: what a wake costs beside, and the
   reader's 0.6%.

   **Measured at `4360662`**, at a load of 12.5 to 29, the heaviest of
   the day: a change answered in 148 ms at the median and 437 ms at
   most, all ten; the sync 0.32 s (0.24 s on a processor); the query as
   the user as root's. The writer's time a wake beside its work, 30 to
   80 µs against 160. Idle 1.00 s in 30 s (3.3%): the writer 0.75 s, the
   reader 0.24 s, through a storm of 67,303 events in one save interval,
   most in folders no index holds; finding them took 372 ms, some 5.5 µs
   an event, and reading them some 3.6 µs. The account does not bound
   finding, which cannot wait. Read back 10.1 s, the home's index loaded
   in 4.9 s against 0.6 s at a lower load. Still to see: what an event
   costs, read and found, profiled in the root tests' machine.

   **The storm's events profiled** (the user's choice: make each event
   cheaper). In the root tests' machine, the writer 60% of the service's
   time, the reader 40%, mostly the kernel giving each event. The writer
   now looks each event's folder up once, not twice, and copies no
   handle: 2 to 4% less of its time (DESIGN 8, "An event costs one
   look"). Not enough for the idle budget through a storm; what remains
   is the kernel's part, the wakes and the allocations.
   Then the reader's wait after a read that gave events, 10 ms to 50:
   through the storm, the writer and the reader take three times less,
   about 2.9 µs an event against 9, and a change in a quiet folder 40 ms
   more, 150 ms at the median (DESIGN 8, "A wake for many events"). To
   be measured on the user's machine.

   **Measured at `9196a7f`**, at a load of 4 to 6.6: a change answered in
   167 ms at the median and 455 ms at most, all ten; idle 0.47 s in 30 s
   (1.6%), met: the writer 0.42 s, the reader 0.03 s against 0.19 to
   0.24 s before the longer wait; the query as the user 1.09 times
   root's; read back 1.13 s; builds 7.4 to 7.7 s. Every budget met but
   the sync: 0.13 s against 0.1, 0.09 s of it on a processor, the
   applications it takes at once among it (27 ms in its interval, a
   folder of 59,469 entries read in 26 to 45 ms). No storm came in this
   run: what one costs on the user's machine is still to be measured.

The user's choices for this phase (2026-11-22): the roots (DESIGN 1),
modes and ACLs evaluated by the service (DESIGN 4), each user's files
read (DESIGN 5). A finding that contradicts a decision is recorded
here as a question, and the work goes on with what does not depend on
it; phase 4, under Wine, if nothing of phase 3 is left.

## Phase 4: Windows

- A user's engine first: the walk and `ReadDirectoryChangesW`. A client
  starts it with no console window (`CREATE_NO_WINDOW`, as skwir's
  ADR-074 starts every program), from a folder it holds nothing busy
  in.
- Then the service: the file table and the change journal. A position
  kept in the journal is used only if the journal's id is the same, and
  the position is at least its lowest valid USN and not past its next
  (`FSCTL_QUERY_USN_JOURNAL`). Otherwise the check runs (DESIGN 2).
- Tried under Wine as far as Wine answers.

### Steps

Begun on 2026-11-23, while skwir's session builds its provider (step B,
above).

1. **Wine, for the tests** (done on 2026-11-23).
   - The development shell gains MinGW's cross compiler and Wine (from
     nixpkgs). `tools/wine-tests` builds the workspace's tests for
     `x86_64-pc-windows-gnu` and runs them under Wine. The gate runs it.
   - At first, 37 tests run there and pass, the protocol's, the index's,
     the crawl's and the client's: whatever needs no folder read yet.
     Two had taken Unix's paths for every system's:
     - the client's test of XDG folders named `/home/...` as absolute;
     - `mounts::lexical` refused every path with a prefix (`C:`), so
       every Windows path. It now keeps the prefix, tested on both.

     The rest is `cfg(target_os = "linux")` until steps 2 to 5 give it
     Windows' code.
   - Wine runs in a prefix of its own under `~/.cache/skwird-tests/wine`,
     with a `HOME` of its own there. Wine links a prefix's Documents,
     Desktop and the like to its user's home folders, and no test may
     reach the user's own.
   - What Wine is not, so what its tests cannot prove:
     - it is not NTFS: its files are the host's;
     - its file ids are the host's inodes;
     - its `ReadDirectoryChangesW` runs over the host's inotify;
     - it has no change journal and no file table to read.

     Those wait for a Windows machine (DESIGN's open question 7).
2. **The walk** (`skwird_crawl`, Windows).
   - A folder is opened by a handle relative to its parent's
     (`NtCreateFile` with the parent as its root directory), never
     through a reparse point. A link, a junction and a volume mounted in
     a folder are not crossed, as a link or a mount is not on Linux.
   - It is listed in batches through that handle
     (`GetFileInformationByHandleEx`, `FileIdExtdDirectoryInfo`): each
     entry's name, kind, file id, size and times, with no open per
     entry.
   - A file's identity is its volume's serial and its file id.
   - Paths of any length (`\\?\`).
   - Private folders and exclusions as on Linux.
   - The test kit runs on Windows (`Scratch` under the user's local
     application data, the reference walk), and the crawl's tests run
     on both systems against it.

   Done on 2026-11-23, with step 3's check (`crates/crawl/src/windows.rs`):
   - **The split.** What every system does (the crawl's scheduling, the
     folders stored, the check's verdicts gathered) moved to `walk.rs`.
     Each system gives only opening, listing and stat-ing a folder.
     Linux's code moved unchanged.
   - **Names** are kept as their WTF-8 (`wtf8.rs`, skwird's own): UTF-8
     for every valid name, an unpaired surrogate kept. The index's log
     and its paths use it too.
   - **Opening** is relative to the root's handle (`NtCreateFile`, with
     `OBJ_DONT_REPARSE` and `FILE_OPEN_REPARSE_POINT`), in parts past
     30,000 units. Then the folder opened must be the one named: its path
     within its volume, links resolved (`GetFinalPathNameByHandleW`), is
     the root's and the path asked.
     - That second guard is the only one under Wine. Wine crosses a link
       in a path's middle whatever it is asked, and the test of a folder
       replaced by a link failed without it.
     - On Windows it costs one call a folder, to measure there.
   - **Listing** is by `NtQueryDirectoryFile` with
     `FileIdFullDirectoryInformation`: 64-bit file ids. Wine lacks
     `FileIdExtdDirectoryInfo`, which has ReFS's 128 bits (open question
     7).
   - **A path** is named plainly (`C:\Users`), as clients name it, and
     opened verbatim (`\\?\C:\Users`), rebuilt from its components,
     since a verbatim path takes `\` alone as a separator.
     `skwird_crawl::canonical` resolves paths so, exclusions among them.
   - **Reparse points**, told by the tag the listing gives:
     - a symbolic link, a junction, Linux's subsystem's link or an app's
       alias is a link, not followed. Wine gives no tag, so any reparse
       point is a link there;
     - a junction that mounts a volume (`\??\Volume{`) is a mount, not
       covered;
     - a folder that any other filter driver gives (cloud files, a
       projected file system) is read where its listing is on the
       machine. It is left out, and not covered as a network mount is,
       where Windows says its provider would fetch it. That is the user's
       choice of 2026-11-23 (DESIGN's open question 11). Each check reads
       such a folder again, so one brought onto the machine is listed
       then.
   - **The check** opens each folder and compares its identity, volume
     and change time through its own handle. That is an open and three
     queries a folder. `NtQueryInformationByName` would take one, where
     Windows has it: to measure on a Windows machine, with open question
     7.
   - **Under Wine:** 52 tests pass, the crawl's 24 among them.
     - The shared tests run on both systems: a crawl equal to the
       reference walk, names alone, private and excluded folders, the
       check's verdicts, the tick, a folder replaced by a link, a
       cancelled crawl, and depth past 260 characters.
     - Wine cannot hold a name with an unpaired surrogate, so under Wine
       the odd name is a surrogate pair. The test kit tells Wine apart
       (`under_wine`).

   Left for later steps:
   - the protocol's `WirePath` reads names lossily off Unix, where they
     are to be WTF-8 (step 4);
   - each listing's entry is an allocation of its own, where Linux's
     read in place (DESIGN 8), to measure first.
3. **The check.** Each folder's identity and change time are compared
   with what was stored, the time read through the folder's own handle:
   on NTFS, the copy in its parent's listing may lag. Whether NTFS's
   folder times can serve so is open question 7. Wine answers it for
   its own files only. Built with step 2.
4. **The engine on Windows.** The engine's modules are compiled on
   every system, what is Linux's behind `cfg`:
   - its folders (`%LOCALAPPDATA%\skwird`);
   - one engine per user (a lock);
   - the transport: a named pipe, `\\.\pipe\skwird-<SID>`, whose
     security descriptor is the user's alone (DESIGN, Protocol);
   - the client finding it, and starting `skwird.exe` with no console
     window, from a folder it holds nothing busy in;
   - `skwird find`, `coverage`, `sync` and `status`.

   It is tested under Wine as the Linux engine's tests test it.

   **The client's side** (done on 2026-11-23, `skwird_client::pipe`):
   - the user's pipe, its server checked to run as the user before
     anything is said;
   - overlapped reads and writes, so the silence and the Stopper work as
     on Unix;
   - the engine started with no console window, in a process group of
     its own, from the system's folder;
   - the pipe's server side, which the engine is to serve on, and the
     fake engine on it.

   The client takes `windows-sys` on Windows (DESIGN, Protocol), and
   `tools/vendor-client` now copies a crate's dependencies of one
   system. Under Wine, the client's tests of giving up pass over the
   pipe. A pipe another user made cannot be tried under Wine, whose
   processes all run as one user.

   **The engine's side** (done on 2026-11-23):
   - The engine's modules are compiled on Linux and Windows. Socket
     activation, the inotify and fanotify listener, and the system
     service's users stay Linux's; macOS keeps its stub until phase 5.
   - The transport is `transport`: a socket, or the pipe's server.
   - What the engine asks of its system is in `os`: the lock
     (`LockFileEx`, as `flock`), and a thread's processor time
     (`GetThreadTimes`).
   - Its folders are under `%LOCALAPPDATA%\skwird`: `run` (the lock),
     `cache`, `config`, `state`. The home is `USERPROFILE`.
   - The asker of a pipe is the engine's own user, whom the pipe alone
     lets in. It is given no id a user has.
   - Paths are resolved by `skwird_crawl::canonical` throughout the
     engine and its command line. The command line resolved them with
     std's first, so `skwird find` said "outside" under Wine.
   - Windows' default private folders: `.ssh`, `.password-store`,
     GnuPG's in `AppData\Roaming\gnupg`, and Windows' credentials, vault
     and data protection keys. They are the same kinds as DESIGN 5's.

   Tried by hand under Wine, on a scratch tree in the tests' prefix:
   - the engine served its pipe and built its index;
   - `status`, `find` and `coverage` answered ("as of");
   - a file made behind its back was found after a `sync` ("checked");
   - read back on the next start, it went once idle.

   **Its tests** (`crates/skwird/tests/windows.rs`, done on 2026-11-23,
   step 4 done). Each engine gets a home, an application data folder and
   a pipe of the test's own. Under Wine, all pass:
   - an engine answers what a walk finds, Unicode names among them, and
     says a path outside its root is outside;
   - a sync answers with a change made behind its back, checked;
   - a client starts the engine, and the engine goes once idle;
   - the user's file of exclusions and Windows' default private folders
     hold;
   - a second engine waits on the lock, and takes over.

   The start found a fault of Windows' own. std's `Command` gives a child
   every inheritable handle, so the engine a client started held the
   client's output pipe, and a read of that output waited for the engine
   to go idle. The client now starts it with `CreateProcessW`,
   inheriting nothing, and names its log (`SKWIRD_LOG`). Under Wine,
   each process is one of the host's and inherits its parent's
   descriptors whatever Windows is asked, so that end is proven on
   Windows alone. Under Wine the test reads the client's output from a
   file.
5. **Live** (`ReadDirectoryChangesW`).
   - One handle per root watches its whole tree, read on a thread of
     its own.
   - Unlike inotify, it needs no watch per folder. So a user's engine on
     Windows answers every root live, not only the folders its clients
     register (DESIGN 2 is to say so).
   - A notification whose buffer overflowed (`ERROR_NOTIFY_ENUM_DIR`, or
     no entries) lowers the root's coverage at once. A check after it
     brings live back.
   - The sync barrier's cookie, as on Linux.
   - The handle holds the root's folder open, which AGENTS.md's "Hold
     nothing of a filesystem between reads" forbids elsewhere. Opened to
     share reading, writing and deletion, it stops no rename or
     removal. It is let go of when its volume is about to be removed
     (`RegisterDeviceNotification`'s query to remove), then opened
     again.

   Done on 2026-11-23, but for the volume's removal
   (`crates/crawl/src/windows/watch.rs`):
   - **The watcher.** Each root is watched before it is read, by one
     handle on a thread of its own, and is heard whole once opened, as
     the system service hears a marked filesystem.
   - **Its events.** Each names an entry by its path below the root. Its
     folder is found in the index by that path and gathered, or the
     nearest folder above it the index holds, for a folder made with
     what is in it. An event in a folder not covered is dropped: the
     engine's own folders are under the home on Windows, and it would
     hear its own saves.
   - **An overflow** lowers the root's coverage, and a check brings live
     back. A watch that ends says so, and its root is checked, never
     live, until the engine starts again.
   - **The barrier.** A cookie is heard in the stream of the root
     holding the cookies' folder alone, and Windows does not order one
     root's stream with another's. So a sync of that root is a barrier,
     and a sync of any other root checks: it answers what was made
     before it, and never waits for a cookie it cannot hear. The
     cookies' folder is in the user's application data, under the home,
     the root a user's engine has by default.
   - **No share.** Registrations (`watch`) change nothing on Windows,
     where every root is live; `watching` says each folder live as its
     root is, with no share.

   Under Wine (its `ReadDirectoryChangesW` runs over the host's inotify),
   all pass: a change, a new deep folder and a removal answered without
   a sync; a sync a barrier where the cookies are heard, and a check
   elsewhere that never waits. An overflow cannot be forced there.

   Left:
   - the volume's removal (`RegisterDeviceNotification`), so that a
     removable drive's root lets go of it;
   - a watch that ended is not tried again until the engine starts
     again.
6. **A logon task** (DESIGN 1): the user's engine started at logon by a
   task of the user's, its definition in `dist/windows/`.

   Done on 2026-11-23:
   - **The task.** `dist/windows/skwird-logon.xml` runs `serve
     --forever` at the user's logon, as the user, in their session, with
     no time limit, below normal priority, one instance, restarted on
     failure. `skwird logon add` registers it for the user running it,
     named by their SID (a task's name is the machine's), with
     `schtasks`; `remove` takes it back.
   - **No console window.** A console program a task starts opens a
     window. The task runs `skwirdw.exe`, skwird built for Windows' GUI
     subsystem, as `pythonw` is: the same command line, taken as a
     module, its log in the engine's state folder. The package builds
     `skwird` alone; `skwirdw` is Windows'.
   - **Watched again.** A root whose watch ended is watched again before
     its next check, and heard whole once the check proves it current. A
     watch whose check fails is let go of, to be tried again.
   - **A race closed.** `watch_root` now asks for the first changes
     before it returns. Its thread asked first, so a change made in
     between was heard by no one, while the root was said heard from
     before it.
   - **Under Wine:**
     - `skwirdw` serves and logs with no console;
     - `logon add` writes the definition (UTF-16, its program and its
       user) and `schtasks` takes it;
     - a root's tree is heard by its paths, three folders deep.

     Not provable there:
     - Wine's `schtasks` keeps no task, and a test that registered one on
       Windows would register it for the user running it: registration
       itself is tried by hand on Windows;
     - Wine keeps a removed root's watch going, so a watch's end is proven
       on Windows alone, and with it the watching again;
     - Wine watches a folder of the tree only once something opened it,
       as the engine's crawl does at once.
7. **The service.**
   - The file table is read whole (`FSCTL_ENUM_USN_DATA`).
   - The change journal is followed (`FSCTL_READ_USN_JOURNAL`) and
     caught up after a restart. Its position is used only where the
     journal's id is the same and the position within its bounds
     (DESIGN 2).
   - Each asker is answered what they could list, checked while
     impersonating the pipe's client (DESIGN 4).
   - A service account.

   Wine has no journal. Its parsing is tested on records of the test's
   making, and the whole on a Windows machine.

   **The volume's sources** (done on 2026-11-23,
   `skwird_crawl::journal`):
   - **`Volume`:** a volume opened itself, which administrators alone may.
     - `enumerate` reads its file table in batches (`FSCTL_ENUM_USN_DATA`).
     - `journal` gives its journal's identity and bounds.
     - `read` gives the journal's records from a position on
       (`FSCTL_READ_USN_JOURNAL`).
   - **Records** of versions 2 (NTFS's 64-bit ids) and 3 (`ReFS`'s 128)
     are read alike, by their own lengths and offsets. A record cut short
     ends a batch and is never read past.
   - **`Table`** assembles the records into the tree their parent links
     make, from a root's id down, each folder before what it holds. What
     no link leads to is set apart, and a loop is never followed twice.
   - **`Position::holds`** is DESIGN 2's rule: the same journal, at or
     past the lowest record it keeps, not past its next. A journal made
     again, one that wrapped past the position, and a position ahead of
     the journal (a volume restored) each make the check run.
   - Under Wine, the parsing, the assembly and the rule pass on records
     of the tests' making. The calls themselves wait for Windows.

   **The file table, read raw** (2026-11-23, on GitHub's Windows runners,
   the fast way the user chose; `skwird_crawl::mft`):
   - **`Volume::table`** reads an NTFS volume's file table whole from the
     disk: where it is (`FSCTL_GET_NTFS_VOLUME_DATA`), its first record's
     data runs, then 1 MiB reads.
   - **`decode`** checks and restores each record by its update sequence.
     A sector written apart from the rest is torn and never read; its
     number is kept as unread. It reads:
     - the times, attributes and security id (`$STANDARD_INFORMATION`);
     - each name and its folder, the short DOS names left out;
     - the data's size, the reparse tag and whether a mount point mounts
       a volume.
   - **`Table`** merges a file's records, places each name in its folder
     only where that folder's record is still the same file, and keeps
     the volume's metadata out.
   - **`TableReader`**, given to the crawl (`crawl_by`), reads each folder
     from the table as `windows::read` reads one from its listing, through
     the same `listed`.
   - **Proven on a 512 MB virtual NTFS disk the runner makes**
     (`skwird_testkit::ntfs`, never the runner's own drive):
     - the table holds what a walk finds (paths, kinds, identities,
       sizes);
     - a crawl read from the table is the crawl read from the folders:
       identities, times, what is left out and why (a `.private` folder,
       an exclusion, a junction, links), entries, sizes and writes.
   - **What the runs taught:**
     - NTFS keeps a folder's kind in its record's head, never in its
       attributes: the table adds it, as a listing does;
     - the two ways of naming a volume (`FILE_ID_INFO`, the table's) are
       the same 64 bits.
     - a raw read sees only what is on the disk. The tests flushed the
       volume before reading its table; the engine did not, so a file
       made a moment before its root was followed could be in neither the
       table nor the journal, the root said current without it (rule 1).
       Found on 2026-11-24, and fixed: `Volume::table_of` flushes the
       volume first (`FlushFileBuffers`), and reads past every cache
       (`FILE_FLAG_NO_BUFFERING`); a volume that cannot be flushed is
       walked. The tests read through it too.
   - **Not yet:** a file table fragmented past its first record (an
     `$ATTRIBUTE_LIST` on record 0) is refused, and the walk serves.
     Every record is held in memory while the table is built: to bound,
     and to measure, on a volume of millions.

   **The journal followed** (2026-11-23, on the runners;
   `Watcher::follow_root`, `windows::watch::follow`):
   - **A root heard through its volume's journal,** read from its next
     record on a thread of its own. It feeds the queue a folder's watch
     feeds, with the same `Heard`, so the engine's live path, its barrier
     and its watching again are the same.
   - **From a position, not a moment:** what is made once the root is
     followed is read whenever the thread reads, never lost before its
     first read.
   - **Idle is idle:** the read waits in the system
     (`FSCTL_READ_USN_JOURNAL` for at least one byte, no time limit),
     beside the thread's stop event. A test counts the reads.
   - **Each record's folder** opened by its identity, once a batch, and
     its path asked:
     - an entry outside the root is not the root's;
     - a folder gone is its own folder's record;
     - one that cannot be opened is a doubt, and an overflow (rule 1);
     - the volume's metadata (its first sixteen records, but the root
       folder's) is passed over, never opened.
   - **Records dropped past the position** are an overflow, read on from
     the next; a journal deleted or made again ends the watch.
   - **A volume with no journal** is given one (32 MiB, by 8 MiB), as
     Everything gives one.
   - **What a run taught:** on a fresh volume Windows writes in
     `System Volume Information` at times of its own, which `LocalSystem`
     alone may open: an overflow, once in five runs. With the backup
     privilege (`skwird_crawl::read_anything`, as the service runs), it
     is opened all the same. Three runs pass since.
   - Proven on a second NTFS disk of the runner's (`T:`), for a test that
     deletes its journal: what is made, written, renamed and removed
     below a root is heard by its path, none of what is outside it, a
     cookie after what came before it; an idle followed root reads
     nothing for two seconds; its watch ends with its journal.

   **What each folder is judged by** (2026-11-23, DESIGN 4, "On
   Windows"), the crawl's side, with `Options::acls`:
   - each folder's security id kept as its owner, from the table or from
     its record read again (`FSCTL_GET_NTFS_FILE_RECORD`);
   - the descriptor each id names fetched once, through a folder holding
     it, where folders are read, never while a client waits; held by
     volume and id (`descriptor_on`);
   - FAT and exFAT, which have no security, listed by anyone
     (`OPEN_TO_ALL`). ReFS is not a root yet.

   Proven on the runner's disk: the ids are the same both ways, two
   folders of the same access share one, a folder given its own access
   another.

   **The asker's token** (2026-11-23):
   - the pipe client's token, taken by impersonating it for the one call
     that opens it, then the server's own again (the process stops if it
     cannot go back); at identification level, enough to judge, never to
     act as it;
   - each folder judged by `AccessCheck`, as Windows judges the asker's
     own open: listing it, passing through it, traverse bypassed where
     the token may (`SeChangeNotifyPrivilege`);
   - judgements kept for the connection by volume, id and access; a
     folder whose descriptor is not held is not known, never kept so;
   - `LocalSystem` sees everything, as root does.

   Proven on the runner with a user the test makes and logs on: it lists
   a folder every user may list, not one the administrators alone may;
   an administrator lists both.

   **The service** (2026-11-23, DESIGN 4, "On Windows";
   `skwird::service`, `skwird::system`'s Windows part):
   - **Registered** by `skwird service add [--root PATH]...`, which starts
     it; `remove` stops it and takes it back. It runs as `LocalSystem`,
     its token kept to the backup, traverse and volume privileges, started
     once the boot is done and again after a failure. Under the service
     manager it runs from its dispatcher, and a stop ends it; from a
     console, it serves in the foreground.
   - **Its folders,** `%ProgramData%\skwird`, its own and the
     administrators' alone; one another user made there is refused.
   - **Its pipe,** `\\.\pipe\skwird`: every user who logs on may connect,
     none but the system and the administrators may make an instance,
     its owner the service's account. A client asks it first and takes
     it only if so owned; a pipe another user made is refused.
   - **Its roots:** every volume of the machine's own that stores files
     (`skwird_crawl::volumes`), never one on a USB, SD or MMC bus. On
     NTFS, each is built from its volume's file table
     (`Keeper::open_by`, `from_table`), and followed through its journal;
     FAT and exFAT are walked and watched.
   - **Its users** are numbered by their SIDs, the numbers kept in its
     state; their homes are their profiles, where the default private
     folders are left out.
   - **Folders above a root,** which no root holds, are passed through by
     a token that may bypass traverse checking, as Windows lets it; one
     that may not is judged by their descriptors (below, "Folders above a
     root").

   - Under Wine, its parts that are not the system's pass: the roots
     chosen from volumes, the numbers kept, a command line read back
     whole. The rest is proven on the runners
     (`crates/skwird/tests/windows_service.rs`, in a test binary of its
     own, since the system's pipe answers every client while it runs):
     the service registered and asked by an administrator and by a user
     the test makes, each answered what they could list. Also there:
     the system's pipe made by an administrator is taken, a user may not
     make an instance of it, and a pipe another user made, open to all,
     is refused.

   **Each user's own exclusions** (2026-11-23): their file and what their
   engine's clients registered, read from their profile's local
   application data as Linux reads them from the home
   (`skwird::system::own_exclusions`, the same on both). Each opened
   beneath the home through no reparse point (`skwird_crawl::open_own`),
   and taken only if its owner is the user, `LocalSystem` or the
   administrators and none but them may write it, by its access list's
   allowing entries. A folder a user keeps out is theirs if its owner is
   them (`skwird_crawl::open_folder`). The service test makes the user's
   file as the user, naming a folder of theirs and one that is not.

   **The journal's position kept with the index** (2026-11-24, DESIGN 2,
   "As built on Windows"):
   - the follower says its position after each batch; the writer takes
     it in once every change heard before it is applied, and never while
     its root proves nothing; the index's manifest keeps it (version 4,
     version 3 still read, as none);
   - at a start where the rule holds, the root is resumed from it: what
     changed meanwhile replayed, live at the replay's end, no check;
   - the service manager's stop ends the engine cleanly, every root saved
     with its position. It ended the process before.

   Tested under Linux: the position saved, read back from the manifest
   alone and with the index, and a manifest of version 3 still read. On
   the runners: a root followed from a position kept hears what was made
   meanwhile, then the replay's end; one of another journal, or ahead of
   it, is not resumed from; the service stopped, a file made, the
   service started again, the file answered with no sync and no check.

   **The check by the table** (2026-11-24): where a position kept does not
   hold, or after an overflow, the system service checks an NTFS root by
   its volume's file table, read after the root is followed
   (`mft::Table::check`, `Keeper::refresh_by`). Each stored folder is
   found by its name from the root's record down and judged as the disk
   would judge it; what is stale is read again from the disk. Unlike a
   build anew, the index's log goes on, and clients' tokens hold. On the
   runners: after a tree changed, the table's check says stale what the
   disk's says; the service, its journal deleted while it was stopped,
   checks by the table at its start, and answers what was made meanwhile.

   **Folders above a root,** judged by their descriptors (2026-11-24): a
   folder above a root that no root holds is read as the service starts,
   as Linux stats it then (`skwird_crawl::judged_by`): its security id
   from its record, its descriptor fetched and held as a root's are, so
   that a token that may not bypass traverse checking passes through it
   only where its descriptor lets it. Tested on a runner with the test
   user's token, that privilege deleted: it passes through a folder every
   user may, not one the administrators alone may; with the privilege, it
   passes through both, unchecked.

   Step 7 is done, its every gap closed.

   **Volumes coming and going** (2026-11-24, DESIGN 4, "On Windows"):
   - the service registers for every volume's arrival and removal
     (`RegisterDeviceNotificationW`, the service's status handle), and the
     writer follows its roots again on each (`Command::Mounts`), and once
     more 3 s after, a volume arrived being given its letter after it is
     told of: a volume found is a root where the roots were not named, one
     gone is let go of, a root whose volume is back checked and followed;
   - each followed root's volume is held, a handle registered: as its
     removal is asked, pending or done, the writer lets go of the root's
     watch and handles before the service answers (`Command::LetGo`), and
     the root is not live until its volume is back. A root whose path is
     gone at any volume's removal is let go of too, whatever its handle
     was told. The registration ends after the manager's call, never from
     within it, which could wait for itself.
   - Tested on a runner: the spare volume a root is on, its virtual disk
     detached while the service runs, is let go of; attached again, the
     root is live. The first run found that a detach asks no one: the
     root never let go, the detach going on all the same. Which of the
     two ways lets go of it now (told as done, or found gone), the passing
     run does not say. Volumes found while it runs
     are not tested there: the runner's own drives would be roots.
   - **What its first run found,** a fault since the service was built:
     an index read back learned no folder's descriptor, which only reading
     a folder learns, so that a folder no change read again since the
     start was denied every asker but `LocalSystem`. A root resumed reads
     again what changed alone, as does a root checked. Each security id
     an index holds has its descriptor fetched once now, as it is read
     back (`Keeper::learn_descriptors`, `Root::learn_all`): one open for
     each descriptor, not each folder. The restart test asks after it for
     a file in the folder only the administrators may list.
8. **Measured,** on a Windows machine when one is at hand: the user's
   choice of which. Until then, Wine's figures are labelled as Wine's,
   never as Windows'.

### Paused (2026-11-23), and how it goes on

Steps 1 to 6 are done, and step 7 is begun: a user's engine on Windows
is built, and tested as far as Wine goes. The user has no Windows
machine. Two things were settled for when it resumed; it resumed the
same day on GitHub's Windows runners (`.github/workflows/windows.yml`),
each a machine thrown away after its job, in place of the virtual
machine below, which stays the way to measure (step 8):
- **The index from the file table: the fast way** (the user's lean, and
  agreed, on 2026-11-23). On NTFS, the service parses the file table's
  own records for sizes, times and security ids, as Everything does,
  rather than reading each folder after a pass of names. The reasons:
  - the walk opens each folder and asks it several questions, so a home
    of 800,000 folders would take minutes cold; the file table is one
    sequential read of the volume, seconds for a million files;
  - where a kept position no longer holds, the table is read again in
    seconds rather than every folder checked, so no folder's change time
    is needed;
  - each record carries the security id that answers per asker need.

  The work is the parser: multi-sector records and their fix-ups,
  records spread over several, hard links, short DOS names left out.
  ReFS, FAT and exFAT have no such table: their volumes are walked.
- **A Windows virtual machine** (`tools/vm/windows`), as the root tests
  have their NixOS machines. Microsoft's evaluation editions are
  licensed for testing:
  - Windows Server 2025 with the desktop, 180 days, needs no TPM;
  - Windows 11 Enterprise, 90 days, needs a virtual TPM and Secure Boot.

  The user downloads the ISO, accepting its terms. The harness:
  - QEMU with KVM, an unattended install that turns OpenSSH on, and a
    snapshot each run starts from;
  - the tests built here for Windows, copied in over SSH, the service's
    run as administrator.

  It costs 30 to 40 GB of disk, and 4 to 8 GB of memory while it runs.
  It would prove what Wine cannot: NTFS's times, ids, journal and file
  table, Task Scheduler, a pipe's security. Its speeds are a guide, not
  a machine's. OneDrive needs an account, so it stays untried there.

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

### Steps

Begun on 2026-11-24, the user's choice, on GitHub's macOS runners
(`macos.yml`, a push to `ci/macos/...`), as phase 4 went on on
Windows': the user has no Mac. A runner is an Apple Silicon machine on
macOS 15, thrown away after its job. The gate lints for macOS
(`aarch64-apple-darwin`), which compiles but links nothing.

1. **The walk and the check** (`skwird_crawl`, macOS).
   - A folder is opened beneath its root's handle by a path that may hold
     no link (`O_NOFOLLOW_ANY`, macOS 11), in place of `O_NOFOLLOW`,
     which macOS refuses beside it (`EINVAL`: the first run read no
     folder), a part at a time past 768 bytes (`PATH_MAX` is 1,024).
     macOS has no `RESOLVE_BENEATH` nor
     `RESOLVE_NO_XDEV`: the path is the names the listings gave, never
     `..`, and a folder opened on another device than its root's is a
     mount.
   - It is listed in batches (`getattrlistbulk`): each entry's name,
     kind, identity, times, size, allocation and count of names, with no
     stat for each entry. An entry whose attributes the listing cannot
     give is stat-ed; one gone since is left out.
   - A mount is told by its device, which each volume has of its own,
     and the table is the system's (`getfsstat` with `MNT_NOWAIT`, which
     waits on no volume). A volume the system says is not on the machine
     (no `MNT_LOCAL`) reaches a network.
   - The check stats each folder by its path, no link followed anywhere
     in it (`AT_SYMLINK_NOFOLLOW_ANY`), through its parent's handle past
     `PATH_MAX`.
   - Its tests: the shared ones (`walk/tests.rs`), and macOS' own: a
     listing's attributes are a stat's, a folder of more than one batch,
     the mount table, and a disk image attached below a root
     (`hdiutil`, which a user may run), told as a mount with the table
     and without it. All pass on the runners (2026-11-24), 91 tests
     with the engine's. APFS keeps names in UTF-8 alone, so the shared
     tests' odd name there is an accent decomposed beside one composed.
2. **The engine on macOS**, a user's: compiled there, its socket, a
   client that starts it; unwatched at first, so that a sync runs the
   check, as phase 1's engine did on Linux.

   Done on 2026-11-24, its tests passing on the runners:
   - the engine's modules are compiled on macOS beside Linux's and
     Windows'. What a watcher does waits for step 3: no watcher is made,
     and a sync checks;
   - who asks, from the socket (`LOCAL_PEERCRED`): the peer's user and
     its groups, 16 at most, which grants a peer in more less, never
     more. A folder's extended ACL is not read yet (step 7): any folder
     may have one, and is not known, never judged by its mode alone;
   - the socket's folder (DESIGN, Protocol, "Transport"): the one
     `XDG_RUNTIME_DIR` names, or the user's own temporary folder, read
     by the client and the engine alike (`skwird_client::runtime_folder`).
     The system service's socket is `/var/run/skwird/socket`;
   - the default private folders gain the keychains
     (`~/Library/Keychains`), where Linux has its keyrings;
   - its tests (`crates/skwird/tests/macos.rs`): the engine answers what
     a walk finds, Unicode names as written; a sync answers a change
     made behind its back, checked after it was asked; a client starts
     the engine, which goes with its socket once idle; the private
     defaults hold. The client's own tests the user's temporary folder:
     absolute, theirs, open to no one else.
3. **Live** (FSEvents): one stream for the user's roots, a folder's
   events heard as a folder to read again, `kFSEventStreamEventFlag
   MustScanSubDirs` and the drops as an overflow of the subtree, the
   stream flushed for a sync (`FSEventStreamFlushSync`). Each stream with
   its generation (DESIGN 2).

   Done on 2026-11-24, on the runners (`crates/crawl/src/macos/watch.rs`):
   - **One stream** for every root and the cookies' folder, so that a
     cookie is heard in its turn after every root's changes before it,
     and a sync of any root is a barrier. Its calls are fsearch's:
     CoreServices loaded on first use, the callback on a queue of
     user-initiated priority, a latency of 0.5 s with `NoDefer`.
   - **An event names a folder:** it is read again, or the nearest the
     index holds above it, read whole. Drops and wrapped ids are an
     overflow of every root; `MustScanSubDirs`, or a volume mounted or
     unmounted below a root, an overflow of that root, which its check
     ends. That is the whole root, where DESIGN 2 asks for the subtree:
     never overstated, but more read than need be, to narrow when a Mac
     measures how often it comes. A root's own folder moved or removed
     (`WatchRoot`), or its volume gone, is lost: not live, and watched
     again by its next check.
   - **A cookie** is a folder of its own with a file made in it, since an
     event names a folder, not an entry; the stream gives what it holds
     back at once (`FlushAsync`), so that a sync waits no latency.
   - **A stream's paths are fixed** when it is made: a root watched, or
     let go of, makes it again from now on, and the one before is made to
     give what it holds back (`FlushSync`), then stopped, so that nothing
     between the two is missed.
   - **What the runs found:**
     - a stream made again from a past event (`sinceWhen`) gives what is
       new before what it replays: the stream's test heard a cookie
       before a change made ahead of it. And the history it replays folds
       a subtree's changes into one to read whole. Hence from now on;
       a position kept (step 4) is replayed while its root proves
       nothing, never as a barrier;
     - the writer set what a user's engine keeps live from its clients'
       scopes after each root opened, on every system but Windows
       (`if cfg!(windows)`), and so undid each root heard whole. One
       constant says it now (`engine::HEARD_WHOLE`), and AGENTS the rule:
       ask the property, not the system.
   - **Tested on the runners:** the stream names a folder changed before
     a cookie made after it; a root moved away is lost; the engine
     answers a change and a removal live without a sync; five syncs in
     turn answer live, each well before the cookie would be given up on;
     a root moved away is not live.
   - **Not tested:** drops and `MustScanSubDirs`, which no test can make
     the system give; a volume mounted below a root while it is heard.
4. **A history** (DESIGN 2): the position kept with the index, replayed
   at the next start, used only where the event database is the same
   (`FSEventsCopyUUIDForDevice`) and the position not ahead of its
   current id; otherwise the check.

   Done on 2026-11-24, on the runners, a user's engine's as the system
   service's will be: any user may replay the events of what they see.
   - **A position** is the root's volume's event database, by its UUID
     folded to 64 bits, and the last event the roots' stream gave. It is
     said as a root is watched (the newest event numbered before its
     stream was made: what came before was there to be read) and after
     each batch. It is kept in the manifest as Windows' is, the event id
     in its signed number: one past it is a position ahead of every
     event, which the next start refuses.
   - **At a start,** a root whose position holds is replayed on a stream
     of its own, from there until its history is given whole
     (`HistoryDone`), beside the roots' stream, and is live then with no
     check. Not in the roots' stream: a replay gives what is new before
     what it replays (step 3). A position of another database, or ahead
     of the current event, is refused, and the root checked.
   - **No position is kept while a root replays:** the roots' stream is
     ahead of the replay, and a position kept past history not replayed
     would lose it at the next start. Windows' follower replays in order
     and keeps its positions as it goes; macOS' cannot.
   - The writer's bookkeeping of histories is Windows' and macOS' alike
     (`Journals`, `replayed`, `positions_applied`); `follow` is each
     system's way to watch a root and resume it.
   - **Tested on the runners:** a position replays a change made after it,
     then its end; one of another database, or ahead of every event, is
     refused; an engine that went, started again, resumes, answers what
     was made while none ran, and is live with no check.
   - **Not tested:** a database made afresh (`/.fseventsd` removed, or a
     volume's events turned off), which needs root.
5. **Consent** (DESIGN 4): folders macOS asks consent for, not covered
   for want of it, and tried again at each start.

   Done on 2026-11-24, on the runners (`skwird_crawl::consent_withheld`):
   - without Full Disk Access (the system's privacy database unreadable,
     fsearch's test, which asks no one), the folders below the home that
     macOS asks consent for (fsearch's list: Desktop, Documents,
     Downloads, the cloud's and other programs' folders, the photo and
     music libraries) are never opened, since opening one asks the user
     and waits for the answer. Each is not covered, as denied, so that a
     client walks it itself; decided once a process, so asked again at
     each start. The engine says so as it starts;
   - decided with it: the system never fetches a file's contents from
     the cloud for the engine (`setiopolicy_np`, dataless files not
     materialized), since nothing skwird does may reach a network
     (rule 6). A folder whose listing the cloud alone holds fails to
     list, and is not covered;
   - tested on the runners, both ways: with Full Disk Access the folders
     are read; without it, denied, unanswered, and said. A passing test
     keeps its output, so the run does not say which way the runner
     went.
6. **A LaunchAgent,** the user's engine started at login.

   Done on 2026-11-24: `skwird logon add|remove`, Windows' command, does
   it on macOS. `add` writes the user's agent
   (`dist/macos/org.skwir.skwird.plist`: `serve --forever`, run at load,
   started again where it failed, a background process of low-priority
   I/O and a nice of 10, as the systemd unit's) to
   `~/Library/LaunchAgents`, and has launchd take it (`launchctl
   bootstrap gui/<uid>`), which starts it now as at each login; `remove`
   takes it back. Tested on the runners: the agent's definition, which
   macOS' own tool reads as a property list (`plutil -lint`). Its
   registration is tested on a runner the workflow marks as thrown away
   alone (`skwird_testkit::throwaway`, Unix's now as Windows'), and
   `macos.yml` does not mark it yet: a change to a workflow, which the
   night's pushes could not make (their token has no `workflow` scope).
7. **The system service,** a LaunchDaemon: `getpeereid` for who asks,
   what each folder is judged by (its mode, and its ACL, which macOS
   keeps extended), Full Disk Access.

   Begun on 2026-11-24:
   - who asks, from the socket (`LOCAL_PEERCRED`, step 2);
   - a folder with an ACL says so where ACLs are asked
     (`ATTR_CMN_EXTENDED_SECURITY`, its count of entries), so that the
     system service never judges it by its mode alone: not known, until
     macOS' ACLs are read. A folder above a root is not known either,
     its ACL unasked. Tested on the runners with an ACL its owner gave
     (`chmod +a`).

   Left: the daemon itself (`skwird service add|remove`, its folders and
   its socket in `/var/run/skwird`), macOS' ACLs read and judged, and
   its tests, which need root on a runner marked thrown away.
8. **Measured,** on a Mac when one is at hand.

## Later

- Packages, beside skwir's: skwir.org's apt and dnf repositories, the
  AUR, Homebrew, winget, and a copy in nixpkgs of the flake's package
  (`dist/nix/package.nix`, written in nixpkgs' manner as skwir's ADR-124
  asks, since phase 3's first step). Whether the name is free there is
  DESIGN's open question 9.

  **Linux's, built and tried** (2026-11-23, DESIGN 11):
  - **The packages:** Debian's and Ubuntu's (cargo-deb), the RPM
    (cargo-generate-rpm) and the AUR's (`dist/aur/PKGBUILD`), each with
    the program, the units and the system service's user, and enabling
    nothing.
    - The scripts: `dist/debian`, `dist/rpm`, `dist/aur/skwird.install`.
    - Removal stops the system service and lets go of its index.
  - **Tried by `tools/package/try.sh`**, without root, in a
    distribution's root entered in a user namespace, as skwir's are. It
    builds the package in one root and installs it into a fresh one with
    the distribution's package manager, which brings what it depends on.
    `tools/package/check.sh` checks it there, as root then as a user:
    - the files and the user;
    - nothing enabled;
    - a user's engine answering a home of synthetic names, and never a
      private folder's;
    - after removal, nothing left.

    Debian 13, Ubuntu 24.04, Fedora 44 and Arch pass.
  - **Not in the gate:** a try downloads a root and a toolchain, and
    takes minutes. It is run when a package's files change, and before a
    release.

  **The version** (the user's choice, the same day, DESIGN 11): skwird's
  own, 0.1.0 for every crate (the workspace's), written out in the
  client's copies by `tools/vendor-client`. The repository is
  `github.com/skwir-org/skwird`, the PKGBUILD's source.

  **Left:**
  - the system service in a package's system, which needs fanotify, so
    a machine (the flake's prove it on NixOS);
  - skwir.org's repositories and their signing, skwir's release tools'
    work, beside skwir's own packages;
  - a release: its tag, notes and assets, and publishing to the AUR and
    nixpkgs, each the user's to do or ask;
  - Homebrew, once macOS has an engine (phase 5), and winget, once
    Windows' is proven on a Windows machine (phase 4, paused).
- CI once there are crates to check on three systems. The development
  shell (`flake.nix`) already carries the Windows and macOS targets.

  **Begun** (2026-11-23), once the repository was made public at
  `github.com/skwir-org/skwird` (AGENTS.md, Testing):
  - **Every push** to `main` or a work branch runs `ci.yml` on Linux:
    formatting, the lints for the three systems, and the tests. Ubuntu's
    rule that refuses unprivileged user namespaces is lifted on the
    runner, for the tests of a mount made below a folder.
  - **On demand,** `windows.yml` and `macos.yml` run the lints and the
    tests on the system itself. Every job fetches every system's crates
    first, since the client's copies are checked offline for Windows
    too.
  - **What it brought:**
    - the 88 tests that pass under Wine pass on Windows Server 2025, on
      NTFS;
    - `skwird logon add` registers its task with Task Scheduler, which
      gives it back, and `remove` takes it back (`throwaway`);
    - macOS builds and runs the 51 tests it has.

    The first runs found no bug of skwird's. A second engine taking over
    once failed to answer in 20 s, in one of four Windows runs, with
    nothing to say why. A wait that fails now prints each engine's log;
    it has not failed again.
