#!/usr/bin/env bash
# The gate every commit passes (docs/PLAN.md, phase 1): formatting, the
# workspace's lints, the tests, the tests that need root in their machine
# (tools/vm/root-tests), and the same lints for Windows and macOS
# (rule 7: every system). Each check stops the gate when it fails.
#
#   tools/gate.sh            run it all
#   tools/gate.sh --no-test  everything but the tests
#   tools/gate.sh --no-machine  all but the root tests in their machine
set -euo pipefail
cd "$(dirname "$0")/.."

run_tests=1
run_machine=1
for arg in "$@"; do
  case "$arg" in
    --no-test) run_tests=0 ;;
    --no-machine) run_machine=0 ;;
    *) echo "usage: tools/gate.sh [--no-test] [--no-machine]" >&2; exit 2 ;;
  esac
done

step() { printf '\n== %s\n' "$*"; }

step "rustfmt"
cargo fmt --all --check
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
for target in x86_64-pc-windows-gnu aarch64-apple-darwin; do
  step "clippy, $target"
  cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
step "the gate holds"
