#!/usr/bin/env bash
# skwird's release (docs/DESIGN.md, decision 11), made on the workstation
# as skwir's are (its ADR-053, 074, 075):
#
#     tools/release/release.sh check [TAG]   the version agrees everywhere
#     tools/release/release.sh build TAG     the bundle, in target/releases
#     tools/release/release.sh verify TAG    the bundle is what it was
#     tools/release/release.sh upload TAG    a GitHub draft of it
#     tools/release/release.sh programs DIR  Windows' and macOS' programs alone, a try
#
# `check` reads files alone, so that a tag's workflow runs it
# (.github/workflows/release.yml). `build` needs the tagged commit checked
# out, its tree clean; it runs the gate and the flake's machines, tries the
# four Linux packages in their roots (tools/package/try.sh), builds
# Windows' and macOS' programs here, their build paths rewritten, and
# stages it all, with checksums, notes and what was checked, in
# target/releases/TAG, which is never replaced. macOS' needs Apple's SDK,
# which skwir's `just macos-sdk` keeps in
# ~/.local/share/skwir-build/macos-sdk (Apple's licence the user's).
# `upload` makes or completes a draft by `gh`: it never publishes one, and
# never moves a tag. Publishing is the user's.
set -euo pipefail
tree="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$tree"

die() {
    echo "release: $*" >&2
    exit 1
}

step() {
    printf '\n== %s\n' "$*"
}

version() {
    sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1
}

# The assets a bundle of version $1 holds, but its checksums: each Linux
# package tried, Windows' programs, macOS' program.
assets() {
    printf '%s\n' \
        "skwird_$1-1_amd64.deb" \
        "skwird_$1-1ubuntu24.04_amd64.deb" \
        "skwird-$1-1.x86_64.rpm" \
        "skwird-$1-1-x86_64.pkg.tar.zst" \
        "skwird-$1-windows-x86_64.zip" \
        "skwird-$1-macos-arm64.tar.gz"
}

# The version agrees everywhere it is written; with a tag, the tag is it.
check() {
    local tag="${1:-}" v first
    v="$(version)"
    echo "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
        die "Cargo.toml's version \"$v\" is not a stable x.y.z"
    if [ -n "$tag" ] && [ "$tag" != "v$v" ]; then
        die "the tag $tag is not v$v, Cargo.toml's version"
    fi
    # Every crate of the workspace, as Cargo.lock holds it.
    local crate locked
    # shellcheck disable=SC2013 # a crate's name a word, none with a space
    for crate in $(awk '/^\[package\]/ { p = 1; next } /^\[/ { p = 0 } p && /^name = / { gsub(/name = |"/, ""); print }' crates/*/Cargo.toml); do
        locked="$(awk -v n="name = \"$crate\"" '$0 == n { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)"
        [ "$locked" = "$v" ] ||
            die "Cargo.lock holds $crate at ${locked:-no version}, not $v: run cargo update --workspace"
    done
    local pkgver
    pkgver="$(sed -n 's/^pkgver=//p' dist/aur/PKGBUILD)"
    [ "$pkgver" = "$v" ] || die "dist/aur/PKGBUILD's pkgver is $pkgver, not $v"
    [ -f CHANGELOG.md ] || die "no CHANGELOG.md"
    first="$(grep -m1 '^## ' CHANGELOG.md || true)"
    echo "$first" | grep -Eq "^## ${v//./\\.} \([0-9]{4}-[0-9]{2}-[0-9]{2}\)$" ||
        die "CHANGELOG.md's first section is \"$first\", not \"## $v (YYYY-MM-DD)\", dated as released"
    echo "release: $v agrees in Cargo.toml, Cargo.lock, the PKGBUILD and CHANGELOG.md ($first)"
}

# The first section of CHANGELOG.md, its heading left out: the release's
# notes.
notes() {
    awk '/^## / { n++; next } n == 1' CHANGELOG.md
}

# What rewrites this machine's paths in what is built: cargo's sources,
# Rust's own, and the tree's, as skwir's builds rewrite them. A panic
# names a place in the source; it never names a folder of the user's.
remap() {
    printf -- '--remap-path-prefix=%s=/cargo\037--remap-path-prefix=%s=/rust\037--remap-path-prefix=%s=/skwird' \
        "${CARGO_HOME:-$HOME/.cargo}" "$(rustc --print sysroot)" "$tree"
}

# Refused if a file built holds this machine's home folder's path.
no_home() {
    local f
    for f in "$@"; do
        if grep -q -a -F "$HOME" "$f"; then
            die "$f holds the path of this machine's home folder"
        fi
    done
}

# Windows' programs, built with MinGW as the tests under Wine are: the
# engine and its window-less twin (skwirdw.exe, which a logon task runs),
# zipped with the licences.
windows() {
    local v="$1" stage="$2" target=x86_64-pc-windows-gnu dir=target/release-build
    local flags
    flags="$(remap)"
    # MinGW's winpthreads, which the shell names for this target: given
    # here too, since encoded flags take the target's place.
    if [ -n "${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS:-}" ]; then
        flags="$flags$(printf '\037')${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS// /$(printf '\037')}"
    fi
    CARGO_TARGET_DIR="$dir" CARGO_INCREMENTAL=0 CARGO_ENCODED_RUSTFLAGS="$flags" \
        cargo build --locked --profile dist --target "$target" -p skwird
    local name="skwird-$v-windows-x86_64"
    rm -rf "${dir:?}/$name" "$dir/$name.zip"
    mkdir -p "$dir/$name"
    cp "$dir/$target/dist/skwird.exe" "$dir/$target/dist/skwirdw.exe" LICENSE-MIT LICENSE-APACHE "$dir/$name/"
    no_home "$dir/$name/skwird.exe" "$dir/$name/skwirdw.exe"
    (cd "$dir" && zip -q -r -X "$name.zip" "$name")
    mv "$dir/$name.zip" "$stage/"
}

# macOS' program for Apple silicon, built with Apple's SDK and LLVM's
# Mach-O linker (the release shell's), signed ad hoc, as Apple silicon asks
# of anything it runs, after its symbols were stripped: not notarized.
macos() {
    local v="$1" stage="$2" target=aarch64-apple-darwin dir=target/release-build
    local sdk="${XDG_DATA_HOME:-$HOME/.local/share}/skwir-build/macos-sdk" root
    [ -f "$sdk/env.sh" ] ||
        die "no macOS SDK in $sdk: skwir's \`just macos-sdk\` keeps one there (Apple's licence is yours to accept)"
    root="$(find "$sdk/SDKs" -maxdepth 1 -name 'MacOSX*.sdk' | sort -V | tail -1)"
    [ -n "$root" ] || die "no MacOSX*.sdk in $sdk/SDKs"
    SDKROOT="$root" MACOSX_DEPLOYMENT_TARGET=11.0 \
        CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$sdk/bin/arm64-apple-darwin-cc" \
        CC_aarch64_apple_darwin="$sdk/bin/arm64-apple-darwin-cc" \
        CARGO_TARGET_DIR="$dir" CARGO_INCREMENTAL=0 CARGO_ENCODED_RUSTFLAGS="$(remap)" \
        cargo build --locked --profile dist --target "$target" -p skwird
    local name="skwird-$v-macos-arm64"
    rm -rf "${dir:?}/$name"
    mkdir -p "$dir/$name"
    cp "$dir/$target/dist/skwird" LICENSE-MIT LICENSE-APACHE "$dir/$name/"
    rcodesign sign "$dir/$name/skwird" >/dev/null
    no_home "$dir/$name/skwird"
    tar -C "$dir" --owner=0 --group=0 --numeric-owner -czf "$stage/$name.tar.gz" "$name"
}

build() {
    local tag="${1:?usage: release.sh build TAG}"
    # The release's shell: Apple's linker's tools, rcodesign and zip.
    if [ -z "${XMAC_CLANG:-}" ]; then
        exec nix develop "$tree#release" --command "$0" build "$tag"
    fi
    check "$tag"
    local commit v="${tag#v}" out="target/releases/$tag" stage="target/releases/.$tag.partial"
    commit="$(git rev-parse --verify --quiet "$tag^{commit}")" ||
        die "no tag $tag: tag the release's commit first (git tag -a $tag -m 'skwird $v')"
    [ "$commit" = "$(git rev-parse HEAD)" ] || die "HEAD is not $tag's commit: check it out"
    [ -z "$(git status --porcelain)" ] || die "the tree is not clean: what git status lists is not the tag's"
    [ ! -e "$out" ] || die "$out is there already: a bundle is never replaced"
    rm -rf "$stage"
    mkdir -p "$stage"

    step "the gate"
    tools/gate.sh
    step "the flake's machines"
    nix build .#checks.x86_64-linux.root .#checks.x86_64-linux.system-service \
        .#checks.x86_64-linux.user-service --no-link
    step "the Linux packages, each tried in its distribution's roots"
    rm -f target/package/linux/*
    local d
    for d in debian ubuntu fedora arch; do
        tools/package/try.sh "$d"
    done
    local a
    for a in $(assets "$v" | grep -v 'windows\|macos'); do
        [ -f "target/package/linux/$a" ] || die "no $a among the packages tried"
        cp "target/package/linux/$a" "$stage/"
    done
    step "Windows' programs"
    windows "$v" "$stage"
    step "macOS' program"
    macos "$v" "$stage"

    notes >"$stage/notes.md"
    {
        echo "skwird $v"
        echo "tag $tag, commit $commit"
        echo "made $(date -u +%Y-%m-%dT%H:%M:%SZ) with $(rustc --version)"
        echo "checked: the gate (tools/gate.sh), the flake's machines (root, system-service, user-service), each Linux package installed and checked in a fresh root of its distribution (tools/package/try.sh)"
        echo "built here: Windows' programs (MinGW) and macOS' (Apple's SDK, signed ad hoc, not notarized), their build paths rewritten"
    } >"$stage/manifest.txt"
    # shellcheck disable=SC2046 # one name a line, none with a space
    (cd "$stage" && sha256sum -- $(assets "$v") >SHA256SUMS)
    mv "$stage" "$out"
    echo
    echo "release: $out staged: $(assets "$v" | wc -l) assets, SHA256SUMS, notes.md, manifest.txt"
}

verify() {
    local tag="${1:?usage: release.sh verify TAG}" v out
    v="${tag#v}"
    out="target/releases/$tag"
    [ -d "$out" ] || die "no bundle $out: build it first"
    local a
    for a in $(assets "$v") SHA256SUMS notes.md manifest.txt; do
        [ -f "$out/$a" ] || die "$out holds no $a"
    done
    [ "$(find "$out" -maxdepth 1 -type f | wc -l)" -eq "$(($(assets "$v" | wc -l) + 3))" ] ||
        die "$out holds a file no bundle holds"
    (cd "$out" && sha256sum --quiet -c SHA256SUMS) || die "a file of $out is not what SHA256SUMS says"
    echo "release: $out is as it was built"
}

upload() {
    local tag="${1:?usage: release.sh upload TAG}" v out commit remote
    v="${tag#v}"
    out="target/releases/$tag"
    verify "$tag"
    commit="$(sed -n 's/^tag .*, commit //p' "$out/manifest.txt")"
    remote="$(git ls-remote origin "refs/tags/$tag^{}" | cut -f1)"
    [ -n "$remote" ] || remote="$(git ls-remote origin "refs/tags/$tag" | cut -f1)"
    [ "$remote" = "$commit" ] ||
        die "GitHub's $tag is ${remote:-not there}, not the bundle's commit $commit: push the tag (git push origin $tag)"
    local files=()
    for a in $(assets "$v") SHA256SUMS; do
        files+=("$out/$a")
    done
    if gh release view "$tag" --json isDraft >/dev/null 2>&1; then
        [ "$(gh release view "$tag" --json isDraft --jq .isDraft)" = true ] ||
            die "$tag is published: a published release is never changed here"
        gh release upload "$tag" "${files[@]}" --clobber
        gh release edit "$tag" --notes-file "$out/notes.md"
    else
        gh release create "$tag" --draft --verify-tag --title "skwird $v" \
            --notes-file "$out/notes.md" "${files[@]}"
    fi
    echo "release: $tag is a draft on GitHub; try it on its systems (try.yml), then publish it"
}

case "${1:-}" in
check) check "${2:-}" ;;
programs)
    if [ -z "${XMAC_CLANG:-}" ]; then
        exec nix develop "$tree#release" --command "$0" "$@"
    fi
    mkdir -p "${2:?usage: release.sh programs DIR}"
    windows "$(version)" "$2"
    macos "$(version)" "$2"
    ls -l "$2"
    ;;
build) build "${2:-}" ;;
verify) verify "${2:-}" ;;
upload) upload "${2:-}" ;;
*)
    sed -n 's/^#     //p' "$0" >&2
    exit 2
    ;;
esac
