# Releasing skwird

A release is built and checked on the workstation, as skwir's are (its
ADR-053; `docs/DESIGN.md`, decision 11). Nothing here commits, tags,
pushes or publishes on its own: each of those is the maintainer's.

## Prepare the source

1. Set the version in `Cargo.toml` (the workspace's) and `pkgver` in
   `dist/aur/PKGBUILD`, then `cargo update --workspace`, which writes the
   workspace's crates into `Cargo.lock` at that version.
2. Date the version's section of `CHANGELOG.md`: `## 0.1.0 (2026-11-30)`.
   It must be the first section: it is the release's notes.
3. `tools/release/release.sh check v0.1.0`: the version agrees in all of
   them.
4. Commit, tag the commit (`git tag -a v0.1.0 -m 'skwird 0.1.0'`), and
   push both. The tag's workflow (`release.yml`) checks the same.

## Build

At the tag, its tree clean, on the x86-64 workstation:

```sh
tools/release/release.sh build v0.1.0
```

It runs the gate and the flake's machines, tries the four Linux packages
in their distributions' roots, builds Windows' and macOS' programs here,
their build paths rewritten (a program that still holds this machine's
home folder's path is refused), and stages them with `SHA256SUMS`, the
notes and what was checked in `target/releases/v0.1.0`, which is never
replaced. macOS' program needs Apple's SDK, which skwir's `just
macos-sdk` keeps (Apple's licence is the maintainer's to accept).

`tools/release/release.sh programs DIR` builds Windows' and macOS'
programs alone, to try that part before a tag.

## Upload, try, publish

```sh
tools/release/release.sh upload v0.1.0
gh workflow run try.yml -f tag=v0.1.0
```

`upload` checks the bundle against its checksums and the tag on GitHub
against the bundle's commit, then makes the draft, or completes one; it
refuses a release already published. `try.yml` takes the draft's Windows
and macOS programs onto GitHub's runners, and has each start, answer a
query and start at login there. Once both pass, the maintainer publishes
the draft.

## After

- The Homebrew formula (`skwir-org/homebrew-tap`) takes the release's
  source: its URL and checksum.
- The AUR's PKGBUILD is published, and nixpkgs is asked: the
  maintainer's to do or ask. The AUR's copy takes the checksum of the
  tag's archive (`updpkgsums`); the repository's keeps `SKIP`, which
  `tools/package/aur.sh` needs, building an archive of the tree.
