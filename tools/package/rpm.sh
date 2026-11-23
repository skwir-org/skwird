#!/bin/sh
# The RPM package of skwird (docs/DESIGN.md, decision 11), made on the
# distribution it is for (Fedora, openSUSE…), whose C library it links:
# the engine built for shipping (the `dist` profile, stripped), then
# cargo-generate-rpm, from crates/skwird/Cargo.toml's
# [package.metadata.generate-rpm].
#
#     tools/package/rpm.sh        # into target/generate-rpm/
#
# Needs cargo-generate-rpm and a linker (tools/package/prepare.sh).
set -eu
cd "$(dirname "$0")/../.."
if ! command -v cargo-generate-rpm >/dev/null; then
    echo "cargo-generate-rpm is needed: cargo install cargo-generate-rpm" >&2
    exit 1
fi
cargo build --locked --profile dist -p skwird --bin skwird
cargo generate-rpm -p crates/skwird --profile dist
