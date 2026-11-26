#!/bin/sh
# The AUR's skwird-bin built from the plain Linux build given, as it would
# be from a release's (dist/aur/skwird-bin/PKGBUILD): the archive named as
# the release names it, then makepkg with the PKGBUILD unchanged, in
# target/aur-bin. On Arch, not as root; arguments after the archive go to
# makepkg.
#
#     tools/package/aur-bin.sh skwird-<version>-linux-x86_64.tar.gz
set -eu
archive="$(cd "$(dirname "${1:?an archive of the plain Linux build}")" && pwd)/$(basename "$1")"
shift
root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$(sed -n 's/^pkgver=//p' "$root/dist/aur/skwird-bin/PKGBUILD")"
# Every crate's version is the workspace's (docs/DESIGN.md, decision 11).
crate="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml")"
if [ -z "$crate" ] || [ "$version" != "$crate" ]; then
    echo "skwird-bin's version ($version) is not the workspace's (${crate:-none})" >&2
    exit 1
fi
work="$root/target/aur-bin"
rm -rf "${work:?}"
mkdir -p "$work"
cp "$root/dist/aur/skwird-bin/PKGBUILD" "$root/dist/aur/skwird.install" "$work/"
cp "$archive" "$work/skwird-$version-linux-x86_64.tar.gz"
cd "$work"
makepkg --noconfirm "$@"
