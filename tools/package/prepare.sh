#!/bin/sh
# What building skwird's Linux packages needs, installed as root on the
# distribution they are for (tools/package/try.sh's build root): a C
# toolchain for the linker, rustup's Rust for Debian, Ubuntu and Fedora,
# at the channel the tree's checks run (RUSTUP_TOOLCHAIN, which try.sh
# reads from rust-toolchain.toml; a distribution's may be older than
# skwird needs), and the packaging tool. Arch builds with its own Rust
# (the PKGBUILD's makedepends), as a user of its own, `builder`.
#
#     RUSTUP_TOOLCHAIN=1.99.1 tools/package/prepare.sh debian|ubuntu|fedora|arch
set -eu
PATH="$HOME/.cargo/bin:$PATH"
rust() {
    if ! command -v rustup >/dev/null; then
        curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none
    fi
    # The host's alone: none of the targets the tree's checks add.
    rustup toolchain list | grep -q "^$RUSTUP_TOOLCHAIN" ||
        rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal
}
case "${1:-}" in
debian | ubuntu)
    apt-get update -q
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends \
        build-essential curl ca-certificates dpkg-dev
    rust
    command -v cargo-deb >/dev/null || cargo install --locked cargo-deb
    ;;
fedora)
    dnf -y -q install --setopt=install_weak_deps=False gcc curl ca-certificates
    rust
    command -v cargo-generate-rpm >/dev/null || cargo install --locked cargo-generate-rpm
    ;;
arch)
    pkgbuild="$(dirname "$0")/../../dist/aur/PKGBUILD"
    if [ ! -f "$pkgbuild" ]; then
        echo "no PKGBUILD at $pkgbuild" >&2
        exit 1
    fi
    needs="$(bash -c '. "$1"; printf "%s\n" "${makedepends[@]}" "${depends[@]}"' _ "$pkgbuild")"
    # shellcheck disable=SC2086 # one argument per package named
    pacman -Syu --noconfirm --needed base-devel $needs
    id builder >/dev/null 2>&1 || useradd -m builder
    ;;
*)
    echo "usage: $0 debian|ubuntu|fedora|arch" >&2
    exit 2
    ;;
esac
