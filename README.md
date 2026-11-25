# skwird

An index of file names for Linux, macOS and Windows, kept current by
the system's change notifications and answered over a local socket in
milliseconds. Every answer says how far it can be trusted: live, checked
at a time, a snapshot as of a time, or not covered and why. A program
that asks it can therefore skip walking the disk where skwird proves it
knows, and walk only where it does not.

It runs in one of three ways, with the same engine and protocol:

- a **system service**, covering every user's files and answering each
  asker only what they could list;
- a **user service**, covering one user's files;
- **on demand**, started by its first client and ending a minute after
  its last.

**Status:** phases 1 and 2 built and measured: a user's engine on
Linux, started by its first client or run for the session as a user
service, answering protocol 0. Its index is checked when a client
asks, and the folders its clients register are watched and answered
live. On a home of 6.9 million entries it answers 0.6 s after a start,
holds 144 to 178 MB idle, and builds its index in under 9 s with the
caches warm, at most 598 MB. A change in a live folder is answered in
about 150 ms, and watching as much of the home as its share of watches
allows costs at most 2% of a core. Every budget it was given is met
(DESIGN 10).
Read `docs/DESIGN.md` for the decisions and `docs/PLAN.md` for the
phases.

```
skwird find report --in ~/Documents   # starts the engine if none answers
skwird sync ~                          # prove it current now
skwird coverage ~/Documents            # how far its answers can be trusted
```

To keep folders live, changes in them heard as they come, ask the
engine to watch them. It keeps them across restarts:

```
skwird watch ~/Documents ~/Projects    # live from now on
skwird watch --remove ~/Projects       # checked again
```

### As a user service

Started by a client, the engine goes a minute after its last client. As
a user service, it lives for the session, so that the folders it
watches stay live between clients. `dist/systemd/` holds the unit and
its socket:

```
cp dist/systemd/skwird.service dist/systemd/skwird.socket ~/.config/systemd/user/
# point ExecStart at skwird where it is installed, then:
systemctl --user daemon-reload
systemctl --user enable --now skwird.socket
```

The socket starts the engine at the first client.

### With Nix

The flake gives the package (`nix build`, built as skwir's is) and a
NixOS module, which installs skwird and enables the user service for
every user:

```nix
# flake.nix: inputs.skwird.url = "git+file:/path/to/skwird";
imports = [ skwird.nixosModules.default ];
services.skwird.enable = true;
# services.skwird.session = false;   # installed only: clients start it
# services.skwird.system = true;     # the system service (experimental)
```

And a Home Manager module, for one user, on Linux or macOS, NixOS or
not: skwird installed, their engine for the session (the package's user
units on Linux, a LaunchAgent on macOS, the one `skwird logon add`
writes), and their own exclusions declared:

```nix
imports = [ skwird.homeManagerModules.default ];
services.skwird.enable = true;
# services.skwird.service = false;   # installed only: clients start it
services.skwird.exclude = [ "~/Videos" "/mnt/scratch" ];
```

The system service needs root, so it is the NixOS module's. Inside
NixOS, where NixOS runs skwird's system service or every user's session,
`service` is off by default: one engine per user. Standalone, Home
Manager cannot see the system's, and `service` is yours to turn off
where the system runs skwird's system service.

### From a Linux package

`tools/package/try.sh debian|ubuntu|fedora|arch` builds the package for
a distribution in a root of its own and checks it installed there, into
`target/package/linux`. A package installs the program, the units and
the system service's user, and enables nothing: clients start a user's
engine, and the user service or the system service are enabled as
above, or below (`docs/DESIGN.md`, decision 11). Removing it stops the
system service and lets go of its index.

### As the system service (experimental, phase 3)

`skwird serve --system`, run from boot by `dist/systemd/skwird-system.service`
(or the module's `services.skwird.system`), covers every local filesystem
that stores files, the Nix store left out, each marked whole with
fanotify, so every change is heard without a watch per folder. It answers
every user what they could list, from their user and groups as the
socket gives them. It keeps every user's private folders out of its
index: the default ones in every home, and each user's own
`~/.config/skwird/exclude` and registrations. It runs as a user of its
own with two capabilities alone (`CAP_SYS_ADMIN` for fanotify,
`CAP_DAC_READ_SEARCH` to read every folder), sandboxed, its index its
own. Clients find it first, so a user's engine is no longer needed.

## Measuring

`skwird-bench` measures what an index would cost on a real tree. It
prints counts, sizes and times, never a name or a path, so its output
can be shared. One script runs all of it on your home folder (or the
folder you name) and keeps the output in one file:

```
tools/measure.sh
```

It asks for your password once: to drop the caches before each cold
pass, which slows the machine for a while afterwards, and to try
fanotify, which only root may use on a whole filesystem. In a btrfs
subvolume, fanotify also mounts the filesystem's top-level subvolume,
read-only and attached nowhere, for the seconds it listens. `--no-root`
skips both, and `--only walk|check|fanotify|index|engine|live` runs one step.
The `index` step builds skwird's index of the folder, saves it and
reads it back, and times a few queries; the `engine` step runs a
user's engine on it, with folders of its own in your cache, and times
the first answer (built, then read back), a sync and 30 idle seconds,
with the memory it holds. Both remove what they built. `--only system`,
never run unless named, runs the system service as root over the whole
machine, its folders in your cache, and asks it as you: its first live
answer, a change answered, a sync, your queries filtered, idle and
memory, then read back. It builds the
tools first when cargo is at hand.

The tool's commands, run by hand:

```
target/release/skwird-bench walk ~            # the crawl, and the index's size
target/release/skwird-bench check ~ --cold    # proving names current, cold then warm
sudo target/release/skwird-bench fanotify ~ --seconds 30   # can a system service watch it all?
```

## Building

Stable Rust 1.85 or later. `nix develop`, or direnv, gives the Rust that
`rust-toolchain.toml` pins and shellcheck. `cargo test`, and `cargo
clippy --all-targets -- -D warnings` before a change is done.

## Credits

The index's layout, and much of what skwird will do on macOS, come from
[fsearch](https://github.com/noahdunnagan/fsearch). Its Linux port,
[fsearch-crossplatform](https://github.com/threehymns/fsearch-crossplatform),
showed what watching a home folder with inotify takes. Both are
MIT-licensed; code taken from them keeps their notices.

## License

MIT or Apache-2.0, at your choice (`LICENSE-MIT`, `LICENSE-APACHE`).
