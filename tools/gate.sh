#!/usr/bin/env bash
# The gate every commit passes (docs/PLAN.md, phase 1): formatting, the
# workspace's lints, the tests, the tests that need root in their machine
# (tools/vm/root-tests), the same lints for Windows and macOS (rule 7:
# every system), and the tests built for Windows, run under Wine
# (tools/wine-tests). Each check stops the gate when it fails. Run it in
# the flake's development shell, which has Wine.
#
#   tools/gate.sh               run it all
#   tools/gate.sh --no-test     everything but the tests
#   tools/gate.sh --no-machine  all but the root tests in their machine
#   tools/gate.sh --no-wine     all but the tests under Wine
set -euo pipefail
cd "$(dirname "$0")/.."

run_tests=1
run_machine=1
run_wine=1
for arg in "$@"; do
  case "$arg" in
    --no-test) run_tests=0 ;;
    --no-machine) run_machine=0 ;;
    --no-wine) run_wine=0 ;;
    *) echo "usage: tools/gate.sh [--no-test] [--no-machine] [--no-wine]" >&2; exit 2 ;;
  esac
done

step() { printf '\n== %s\n' "$*"; }

step "rustfmt"
cargo fmt --all --check
step "third-party notices"
tools/notices check
step "clippy, this system"
cargo clippy --workspace --all-targets -- -D warnings
if [ "$run_tests" = 1 ]; then
  step "tests"
  cargo test --workspace
  if [ "$run_machine" = 1 ]; then
    step "tests that need root, in their machine"
    tools/vm/root-tests > target/vm-tests.log 2>&1 || { tail -40 target/vm-tests.log; exit 1; }
    sed 's/\x1b\[[0-9;]*m//g' target/vm-tests.log | grep "^test result"
  fi
fi
# Built once a gate, so no incremental cache is kept for them: Windows'
# alone took 3 GB of a disk (AGENTS.md, "Disk").
for target in x86_64-pc-windows-gnu aarch64-apple-darwin; do
  step "clippy, $target"
  CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
# Each program alone, as a release builds it: a build of every target, or
# of the whole workspace, takes the features of the tests' kit too, and a
# feature a crate forgot passes there (`2164a77`, which built no skwird for
# Windows).
for target in "" x86_64-pc-windows-gnu aarch64-apple-darwin; do
  for program in skwird skwird-bench; do
    step "$program alone${target:+, $target}"
    CARGO_INCREMENTAL=0 cargo check --quiet -p "$program" ${target:+--target "$target"}
  done
done
if [ "$run_tests" = 1 ] && [ "$run_wine" = 1 ]; then
  step "tests for Windows, under Wine"
  tools/wine-tests > target/wine-tests.log 2>&1 || { tail -40 target/wine-tests.log; exit 1; }
  grep "^test result" target/wine-tests.log | awk '{ p += $4; f += $6 } END { print p " passed, " f " failed" }'
fi
step "the gate holds"
