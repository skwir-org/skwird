#!/bin/sh
# Debian's and Ubuntu's package of skwird (docs/DESIGN.md, decision 11),
# made on the distribution it is for, whose C library it links: the
# engine built for shipping (the `dist` profile), then cargo-deb, from
# crates/skwird/Cargo.toml's [package.metadata.deb].
#
#     tools/package/deb.sh        # into target/debian/
#
# Made on Ubuntu, its revision names the release
# (skwird_0.1.0-1ubuntu24.04_amd64.deb), so Ubuntu's is named apart from
# Debian's (skwird_0.1.0-1_amd64.deb). Needs cargo-deb and a linker
# (tools/package/prepare.sh).
set -eu
cd "$(dirname "$0")/../.."
if ! command -v cargo-deb >/dev/null; then
    echo "cargo-deb is needed: cargo install cargo-deb" >&2
    exit 1
fi
cargo build --locked --profile dist -p skwird --bin skwird
revision=1
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091 # the system's, read where it runs
    os="$(. /etc/os-release && echo "${ID:-}:${VERSION_ID:-}")"
    case "$os" in
    ubuntu:?*) revision="1ubuntu${os#ubuntu:}" ;;
    esac
fi
cargo deb --locked -p skwird --no-build --deb-revision "$revision"
