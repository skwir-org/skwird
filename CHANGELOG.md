# Changelog

skwird's releases. Each section is its release's notes on GitHub
(`tools/release/release.sh`): the first is the release being made, dated
the day it is released.

## 0.2.0 (unreleased)

- **Names in both Unicode forms:** a term is matched composed and
  decomposed (NFC and NFD), as skwir matches it, so that `café` typed
  finds a name macOS wrote decomposed, and back. A glob is normalized in
  its literal parts alone. No name is rewritten, and the index is
  unchanged. The engine says so in its hello (`names-both-forms`).
- **A sync answers at once on Windows and macOS:** on Windows, a sync of
  any root is a barrier, where one of a root that did not hold the
  engine's folders checked it whole; on macOS, a sync waits at most the
  stream's 50 ms latency, not 0.5 s.
- **macOS gives back the memory it freed** after a build, as Linux does.

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
