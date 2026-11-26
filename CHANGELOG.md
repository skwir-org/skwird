# Changelog

skwird's releases. Each section is its release's notes on GitHub
(`tools/release/release.sh`): the first is the release being made, dated
the day it is released.

## 0.3.0 (unreleased)

- **`skwird stop`:** the user's engine stops as when idle, what it heard
  applied and its index saved, where it runs this copy of skwird (a
  program in the same folder); the command gives back once it ended, 10
  s at most, so that an installer may replace its files. An engine of
  another copy is left running, and named. On every system; the system
  service is its service manager's to stop. A request of the protocol,
  `stop`, which the engine says in its hello.
- **`skwird logon remove --if-mine`** (Windows, macOS): the logon task,
  or the LaunchAgent, taken back only where it runs this copy of skwird,
  its engine stopped first; one that runs another copy's is left, and
  named. `logon remove` alone takes back whichever, as before.
- **Third-party notices** beside every program: each crate built into
  it, its licence and its licence files' texts, Rust's standard library,
  and on Windows MinGW-w64's runtime (`THIRD-PARTY-NOTICES`, in the
  Windows and macOS archives, and where each package keeps its
  licences).
- **Quiet on Windows:** `skwird logon` starts `schtasks` with no console
  window, and `skwirdw.exe` makes its state folder and log only to
  serve: an installer runs it unseen, and leaves nothing of skwird's.

## 0.2.0 (2026-11-25)

- **Names in both Unicode forms:** a term is matched as typed, composed
  and decomposed (NFC and NFD), as skwir matches it, so that `café`
  typed finds a name macOS wrote decomposed, and back, and a character
  both forms change (U+F900) finds itself. Where case is ignored, a
  final sigma is a sigma: `ΟΔΟΣ` finds `ΟΔΟΣ.txt`. A glob is normalized
  in its literal parts alone, its escapes and classes folded where case
  is ignored. No name is rewritten, and the index is unchanged. The
  engine says so in its hello (`names-both-forms`).
- **A sync answers at once on Windows and macOS:** on Windows, a sync of
  any root is a barrier, where one of a root that did not hold the
  engine's folders checked it whole; on macOS, a sync waits at most the
  stream's 50 ms latency, not 0.5 s.
- **macOS gives back the memory it freed** after a build, as Linux does.
- **Windows maps its index** as Linux and macOS do: its own memory at
  rest is 5.6 to 9.5 MB for a million entries on the runner, not 40 to
  49.
- **Removable volumes, on request:** a client keeps a volume by its
  identity once its user said yes (`volumes`); a kept volume is indexed
  whenever it is mounted, in the engine's cache, nothing written on it,
  at the system's lowest I/O priority. Released before an eject, by its
  identity or where it is mounted, it is answered once nothing of it is
  held. On Linux, macOS and Windows; the system service keeps none. The
  engine says so in its hello (`volumes`).
- **Nix:** a Home Manager module (`services.skwird`), for one user on
  Linux or macOS: the package, their engine for the session, their
  exclusions declared. The flake's package is offered on macOS too.
- **Found by a review before this release,** among others:
  - Linux's system service was never live on a btrfs mounted through a
    subvolume (Fedora's `/home`): its unit hid the block devices its mark
    needs.
  - On Windows and macOS, an exclusion named before its folder existed
    kept out no folder made with another case (`secret`, `Secret`).
  - A client started a second engine beside one that answered with an
    error, and Windows' pipe had no instance between two clients.
  - On macOS, a volume mounted during a crawl was entered.
  - On Windows, a root renamed stayed live, a change said by a short
    name (`LONGFO~1`) was applied nowhere, and a file table holding
    records it could not read was built from.
  - `changes` never told the folder asked for removed.
  - An answer could be longer than a client reads: results are batched
    by size, and `changes` past a line is answered too old.

## 0.1.0 (2026-11-24)

The first release of skwird: an index of file names kept current by each
system's change notifications, answered over a local socket. A client asks
it what is where, and every answer says how far it can be trusted: live,
checked as of a time, or not covered, and why. skwir is its first client.

- **Linux:** a user's engine, started by its first client or at login by
  a systemd user unit, heard live by inotify; the system service, which
  hears whole filesystems by fanotify and answers each user what they
  could list. Packages for Debian, Ubuntu, Fedora and Arch (the AUR).
- **Windows:** a user's engine, started at logon by a task of the user's
  (`skwird logon add`); the system service (`skwird service add`, as an
  administrator), which reads each volume's file table and follows its
  journal, resumed from at each start.
- **macOS:** a user's engine, heard live by FSEvents and resumed from its
  history at each start, started at login by a LaunchAgent (`skwird logon
  add`); the system service, a LaunchDaemon (`sudo skwird service add`),
  which judges each folder's ACL as the kernel does. Without Full Disk
  Access, the folders macOS asks consent for (Desktop, Documents,
  Downloads and others) are not read, and said not covered.
- **Private folders are never indexed:** `.ssh`, `.gnupg` and the
  keychains among them, and each user's own exclusions.
- **Idle is idle:** no polling, and nothing leaves the machine.

The protocol is experimental (version 0): a later release may change it.

Windows and macOS were tested on GitHub's runners, not on machines of
their own. Their programs here are built on Linux, and were run on the
runners before this release was published. macOS' is signed ad hoc, not
notarized: macOS refuses it the first time it is opened, until it is
allowed in System Settings, Privacy & Security. Homebrew builds it from
source instead (`skwir-org/tap`).
