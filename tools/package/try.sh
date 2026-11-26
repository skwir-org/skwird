#!/bin/sh
# skwird's Linux package built and installed in a distribution's own
# root, from any Linux with bubblewrap (docs/DESIGN.md, decision 11): no
# container engine, no root, nothing of this system touched, as skwir's
# packages are tried (its ADR-040).
#
# The distribution's root filesystem (images.linuxcontainers.org, checked
# against the sums published beside it) is unpacked under
# ~/.cache/skwird-tests/roots and entered as its root user in a user
# namespace. What the build needs is installed there on every try
# (prepare.sh); the tree's files are built into the package (deb.sh,
# rpm.sh, aur.sh); then a fresh root installs it with the distribution's
# package manager, and check.sh checks it there: the files, as root; the
# engine, as a user; then the package removed. The package lands in
# target/package/linux once it passed.
#
# Two more (docs/PLAN.md, 0.3.0, step 5):
# - `linux`, the plain Linux build (linux.sh), built in Ubuntu 22.04's
#   root, the oldest C library it is built against; installed by hand,
#   as its INSTALL.txt says, in a fresh root of that Ubuntu and of each
#   distribution above, and its engine checked in each;
# - `arch-bin`, the AUR's skwird-bin (aur-bin.sh), built in Arch's root
#   from the plain build a `linux` try left, and tried as `arch` is.
#
#     tools/package/try.sh debian|ubuntu|fedora|arch|linux|arch-bin
set -eu
distribution="${1:-}"
case "$distribution" in
debian | ubuntu | fedora | arch | linux) family="$distribution" ;;
arch-bin) family=arch ;;
*)
    echo "usage: $0 debian|ubuntu|fedora|arch|linux|arch-bin" >&2
    exit 2
    ;;
esac
tree="$(cd "$(dirname "$0")/../.." && pwd)"
roots="$HOME/.cache/skwird-tests/roots"
out="$tree/target/package/linux"
mkdir -p "$roots" "$out"
exec 9>"$roots/$family.lock"
flock -n 9 || { echo "a $family package build is already running" >&2; exit 1; }
# The Rust the tree's checks run, for the packages built with rustup's.
toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$tree/rust-toolchain.toml")"
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' "$tree/Cargo.toml")"
plain="skwird-$version-linux-x86_64.tar.gz"

# The image of the distribution $1, once: its root filesystem, checked
# against its sum. Its path.
image() {
    case "$1" in
    debian) at=debian/trixie/amd64/default ;;
    ubuntu) at=ubuntu/noble/amd64/default ;;
    fedora) at=fedora/44/amd64/default ;;
    arch) at=archlinux/current/amd64/default ;;
    linux) at=ubuntu/jammy/amd64/default ;;
    esac
    file="$roots/$1.rootfs.tar.xz"
    if [ ! -f "$file" ]; then
        index="https://images.linuxcontainers.org/images/$at"
        build="$(curl -fsSL "$index/" | grep -o 'href="[0-9][^"]*/"' | tail -1 | cut -d'"' -f2)"
        curl -fsSL -o "$file.part" "$index/${build}rootfs.tar.xz"
        sum="$(curl -fsSL "$index/${build}SHA256SUMS" | awk '$2 == "rootfs.tar.xz" { print $1 }')"
        if [ "$(sha256sum "$file.part" | cut -d' ' -f1)" != "$sum" ]; then
            echo "$file.part: not the image's published sum" >&2
            exit 1
        fi
        mv "$file.part" "$file"
    fi
    echo "$file"
}

# A root of the distribution $2 at $1, as unpacked by someone who is not
# root: folders their owner may write, this system's DNS, and the package
# managers' own sandboxes off (they would drop to users a namespace of one
# user does not have).
unpack() {
    tarball="$(image "$2")"
    if [ -d "$1" ]; then
        chmod -R u+rwX "$1"
        rm -rf "${1:?}"
    fi
    mkdir -p "$1"
    tar -xJf "$tarball" -C "$1" --exclude='./dev/*'
    chmod u+rwx "$1" "$1/root"
    rm -f "$1/etc/resolv.conf"
    cp -L /etc/resolv.conf "$1/etc/resolv.conf"
    case "$2" in
    debian | ubuntu | linux)
        echo 'APT::Sandbox::User "root";' > "$1/etc/apt/apt.conf.d/99skwird-sandbox"
        ;;
    arch)
        sed -i 's/^CheckSpace/#CheckSpace/; s/^DownloadUser/#DownloadUser/; s/^#\(DisableSandbox\)/\1/' "$1/etc/pacman.conf"
        # shellcheck disable=SC2016 # pacman expands $repo and $arch
        printf 'Server = %s/$repo/os/$arch\n' \
            https://geo.mirror.pkgbuild.com \
            https://fastly.mirror.pkgbuild.com \
            http://mirrors.kernel.org/archlinux > "$1/etc/pacman.d/mirrorlist"
        ;;
    esac
}

# Run "$@" in the root $1 as its root user (uid 0) or as `builder`
# (1000), with the tree's tools at /tools and what packages are made of
# at /dist, and this system's crates cache shared. Nothing else of this
# system's is there: no home, no runtime folder, so no engine of this
# system's user's is reached.
enter() {
    root="$1"
    uid="$2"
    shift 2
    if [ "$uid" = 0 ]; then
        package_home=/root
        set -- --cap-add ALL "$@"
    else
        package_home=/home/builder
    fi
    mkdir -p "$root$package_home/.cargo/registry" "$root$package_home/.cargo/git" "$HOME/.cargo/registry" "$HOME/.cargo/git"
    bwrap --unshare-user --uid "$uid" --gid "$uid" --unshare-pid --share-net --die-with-parent \
        --bind "$root" / --proc /proc --dev /dev --tmpfs /tmp \
        --ro-bind "$tree/tools" /tools --ro-bind "$tree/dist" /dist \
        --bind "$HOME/.cargo/registry" "$package_home/.cargo/registry" \
        --bind "$HOME/.cargo/git" "$package_home/.cargo/git" \
        --clearenv --setenv HOME "$package_home" --setenv LANG C.UTF-8 \
        --setenv PATH "$package_home/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        --setenv RUSTUP_TOOLCHAIN "$toolchain" \
        --setenv CARGO_BUILD_JOBS "${CARGO_BUILD_JOBS:-8}" \
        "$@"
}

# The root $1 of the distribution $2 made fit for a user namespace.
rootless() {
    case "$2" in
    debian | ubuntu | linux) enter "$1" 0 /tools/package/rootless-apt.sh >/dev/null ;;
    esac
}

# The build root, unpacked once and kept; what the build needs installed
# on every try, which passes over what is there in a second or two.
builder="$roots/$family-build"
if [ ! -f "$builder/.skwird-prepared" ]; then
    unpack "$builder" "$family"
    if [ "$family" = arch ]; then
        enter "$builder" 0 sh -c 'pacman-key --init && pacman-key --populate archlinux' >/dev/null
    fi
    rootless "$builder" "$family"
    touch "$builder/.skwird-prepared"
fi
enter "$builder" 0 /tools/package/prepare.sh "$family"
chmod u+rwx "$builder" "$builder/root"

# The tree's files, fresh; the build's target kept between tries.
if [ -d "$builder/src" ]; then
    chmod -R u+rwX "$builder/src"
    rm -rf "${builder:?}/src"
fi
mkdir -p "$builder/src"
(cd "$tree" && git ls-files -co --exclude-standard -z | tar --null -T - -cf -) | tar -xf - -C "$builder/src"
case "$distribution" in
debian | ubuntu)
    rm -rf "$builder/build/target/debian"
    enter "$builder" 0 env CARGO_TARGET_DIR=/build/target sh -c 'cd /src && tools/package/deb.sh'
    made="$builder/build/target/debian"
    pattern='skwird_*.deb'
    ;;
fedora)
    rm -rf "$builder/build/target/generate-rpm"
    enter "$builder" 0 env CARGO_TARGET_DIR=/build/target sh -c 'cd /src && tools/package/rpm.sh'
    made="$builder/build/target/generate-rpm"
    pattern='skwird-[0-9]*.rpm'
    ;;
arch)
    enter "$builder" 1000 sh -c 'cd /src && tools/package/aur.sh'
    made="$builder/src/target/aur"
    pattern='skwird-[0-9]*.pkg.tar.zst'
    ;;
arch-bin)
    [ -f "$out/$plain" ] || { echo "no $out/$plain: try.sh linux makes it" >&2; exit 1; }
    cp "$out/$plain" "$builder/src/"
    enter "$builder" 1000 sh -c "cd /src && tools/package/aur-bin.sh /src/$plain"
    made="$builder/src/target/aur-bin"
    pattern='skwird-bin-[0-9]*.pkg.tar.zst'
    ;;
linux)
    rm -rf "$builder/build/target/linux"
    enter "$builder" 0 env CARGO_TARGET_DIR=/build/target sh -c 'cd /src && tools/package/linux.sh'
    made="$builder/build/target/linux"
    pattern="$plain"
    ;;
esac

# The plain build: installed by hand, as its INSTALL.txt says, in a fresh
# root of each distribution, its files checked as root and its engine as
# a user; then it lands with the packages.
if [ "$distribution" = linux ]; then
    for on in linux debian ubuntu fedora arch; do
        fresh="$roots/linux-on-$on"
        unpack "$fresh" "$on"
        rootless "$fresh" "$on"
        mkdir -p "$fresh/package"
        cp "$made/$plain" "$fresh/package/"
        enter "$fresh" 0 sh -c "cd /package && tar -xzf $plain && install -Dm755 skwird-*/skwird /usr/local/bin/skwird"
        echo "== linux, on $on: installed by hand"
        enter "$fresh" 0 /tools/package/check.sh plain "/package/${plain%.tar.gz}"
        echo "== linux, on $on: a user's engine"
        enter "$fresh" 1000 /tools/package/check.sh engine
        chmod -R u+rwX "$fresh"
        rm -rf "${fresh:?}"
    done
    cp "$made/$plain" "$out/"
    echo "built and checked: $out/$plain"
    exit 0
fi

# A fresh root installs it, with what it depends on alone, and checks it:
# as root, then as a user; then removes it, and checks what is left.
fresh="$roots/$family-try"
unpack "$fresh" "$family"
rootless "$fresh" "$family"
mkdir -p "$fresh/package"
# shellcheck disable=SC2086 # the name is a pattern
for package in "$made"/$pattern; do
    case "$package" in
    *-debug-* | *-dbgsym*) continue ;;
    esac
    cp "$package" "$fresh/package/"
done
case "$distribution" in
debian | ubuntu)
    enter "$fresh" 0 sh -c 'apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends /package/*.deb' >/dev/null
    remove='DEBIAN_FRONTEND=noninteractive apt-get purge -y -q skwird'
    ;;
fedora)
    enter "$fresh" 0 sh -c 'dnf -y -q install --setopt=install_weak_deps=False /package/*.rpm' >/dev/null
    remove='dnf -y -q remove skwird'
    ;;
arch | arch-bin)
    enter "$fresh" 0 sh -c 'pacman-key --init && pacman-key --populate archlinux && pacman -Syu --noconfirm && pacman -U --noconfirm /package/*.pkg.tar.zst' >/dev/null
    remove="pacman -R --noconfirm skwird${distribution#arch}"
    ;;
esac
echo "== $distribution: installed"
enter "$fresh" 0 /tools/package/check.sh installed
echo "== $distribution: a user's engine"
enter "$fresh" 1000 /tools/package/check.sh engine
# The system service's index, as it would have left one, for the removal
# to let go of.
enter "$fresh" 0 sh -c "mkdir -p /var/cache/skwird && : > /var/cache/skwird/index && $remove" >/dev/null
echo "== $distribution: removed"
enter "$fresh" 0 /tools/package/check.sh removed
for package in "$fresh"/package/*; do
    cp "$package" "$out/"
    echo "built and checked: $out/$(basename "$package")"
done
chmod -R u+rwX "$fresh"
rm -rf "${fresh:?}"
