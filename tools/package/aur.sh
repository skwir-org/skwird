#!/bin/sh
# The AUR package built from this tree as it would be from a release
# (dist/aur/PKGBUILD): the tree's files as the release's source tarball
# (`skwird-<version>/…`, as GitHub lays one out), then makepkg with the
# PKGBUILD unchanged, in target/aur. On Arch, not as root; arguments go to
# makepkg (`--syncdeps` installs what it needs through sudo).
#
#     tools/package/aur.sh
set -eu
root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$(sed -n 's/^pkgver=//p' "$root/dist/aur/PKGBUILD")"
# Every crate's version is the workspace's (docs/DESIGN.md, decision 11).
crate="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")"
if [ -z "$crate" ] || [ "$version" != "$crate" ]; then
    echo "the PKGBUILD's version ($version) is not the workspace's (${crate:-none})" >&2
    exit 1
fi
work="$root/target/aur"
rm -rf "${work:?}"
mkdir -p "$work"
cp "$root/dist/aur/PKGBUILD" "$root/dist/aur/skwird.install" "$work/"
# The tracked files and those not ignored, as a release's tag holds them;
# where there is no repository (a build root's copy), all but the build's.
if git -C "$root" rev-parse --git-dir >/dev/null 2>&1; then
    (cd "$root" && git ls-files -co --exclude-standard) > "$work/files"
else
    (cd "$root" && find . -path ./target -prune -o -type f -print | sed 's,^\./,,') > "$work/files"
fi
tar -C "$root" -T "$work/files" --transform "s,^,skwird-$version/," -czf "$work/skwird-$version.tar.gz"
cd "$work"
makepkg --noconfirm "$@"
