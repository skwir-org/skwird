#!/bin/sh
# skwird's plain Linux build (docs/PLAN.md, 0.3.0, step 5): the program
# built for shipping (the `dist` profile) in Ubuntu 22.04's root, whose C
# library is the oldest it is built against (glibc 2.35: it runs on every
# later one), archived with its licences, its notices, the systemd units
# and the sysusers file for a setup by hand, and what that setup takes
# (dist/linux/INSTALL.txt). For AppImage users, other distributions, and a
# program that carries skwird.
#
#     tools/package/linux.sh      # into target/linux/
#
# Run in tools/package/try.sh's build root, as the packages are: its
# paths are the root's alone (/src, /root/.cargo), none of a user's.
set -eu
cd "$(dirname "$0")/../.."
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)"
arch="$(uname -m)"
target="${CARGO_TARGET_DIR:-target}"
cargo build --locked --profile dist -p skwird --bin skwird
name="skwird-$version-linux-$arch"
out="$target/linux"
rm -rf "${out:?}/$name" "$out/$name.tar.gz"
mkdir -p "$out/$name/systemd" "$out/$name/sysusers"
cp "$target/dist/skwird" LICENSE-MIT LICENSE-APACHE README.md dist/linux/INSTALL.txt "$out/$name/"
cp dist/notices/linux.txt "$out/$name/THIRD-PARTY-NOTICES.txt"
cp dist/systemd/skwird.service dist/systemd/skwird.socket dist/systemd/skwird-system.service "$out/$name/systemd/"
cp dist/sysusers/skwird.conf "$out/$name/sysusers/"
tar -C "$out" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"
echo "linux: $out/$name.tar.gz"
