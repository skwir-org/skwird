#!/usr/bin/env bash
# Phase 0's whole measurement (docs/PLAN.md) on one folder, your home by
# default, kept in one file you can share: it holds counts, sizes and
# times, never a name or a path.
#
#   tools/measure.sh [--no-root] [--only walk|check|fanotify] [FOLDER]
#
# It asks for your password once (sudo), to drop the caches before each
# cold pass (the machine is slower for a while after) and to try
# fanotify, which only root may use on a whole filesystem. On a btrfs
# subvolume, fanotify also mounts the filesystem's top-level subvolume,
# read-only and attached nowhere, for the seconds it listens. --no-root
# skips both: warm passes only, no fanotify. --only runs one step. It
# takes a few minutes; an idle machine gives the fairest numbers.
set -euo pipefail

usage='tools/measure.sh [--no-root] [--only walk|check|fanotify] [FOLDER]'
use_root=1
only=
folder=$HOME
while [ $# -gt 0 ]; do
    case $1 in
    --no-root) use_root=0 ;;
    --only)
        case ${2:-} in
        walk | check | fanotify) only=$2 ;;
        *)
            echo "--only takes walk, check or fanotify ($usage)" >&2
            exit 2
            ;;
        esac
        shift
        ;;
    -h | --help)
        sed -n '2,14s/^# \{0,1\}//p' "$0"
        exit 0
        ;;
    -*)
        echo "unknown option $1 ($usage)" >&2
        exit 2
        ;;
    *) folder=$1 ;;
    esac
    shift
done
# Whether a step runs: every step, or the one --only names.
want() { [ -z "$only" ] || [ "$only" = "$1" ]; }
if [ "$(id -u)" = 0 ]; then
    # As root, the home folder and the cache would be root's.
    echo "Run it as yourself: it asks for sudo when it needs it." >&2
    exit 2
fi
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
root=$(realpath "$folder")
bench=$here/target/release/skwird-bench
cache=${XDG_CACHE_HOME:-$HOME/.cache}/skwird-bench
out=$cache/measured-$(date +%Y%m%d-%H%M%S).txt
probe=$cache/probe

if command -v cargo >/dev/null 2>&1 &&
    { [ ! -x "$bench" ] ||
        [ -n "$(find "$here/crates" "$here/Cargo.lock" -newer "$bench" -print -quit)" ]; }; then
    echo "Building skwird-bench…"
    if ! (cd "$here" && cargo build --release --quiet -p skwird-bench); then
        if [ -x "$bench" ]; then
            echo "The build failed: measuring with the tool built before." >&2
        fi
    fi
fi
if [ ! -x "$bench" ]; then
    echo "skwird-bench is not built, and cargo is not here to build it." >&2
    exit 1
fi
mkdir -p "$cache" "$probe"

keeper=
if [ "$use_root" = 1 ]; then
    echo "Your password is asked once: to drop the caches, and to try fanotify."
    if sudo -v; then
        # Keep sudo's ticket while the measurement runs.
        (while kill -0 $$ 2>/dev/null; do
            sudo -n -v 2>/dev/null
            sleep 30
        done) &
        keeper=$!
    else
        echo "No sudo: warm passes only, no fanotify."
        use_root=0
    fi
fi
cleanup() {
    if [ -n "$keeper" ]; then kill "$keeper" 2>/dev/null || true; fi
    rmdir "$probe" 2>/dev/null || true
}
trap cleanup EXIT

drop='sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null'

section() { printf '\n== %s\n' "$1"; }

# A step that fails says so, and the others still run.
run() { "$@" || echo "(failed, exit $?)"; }

# Drop the caches before a cold pass; one that cannot be dropped is said,
# and its pass skipped, since it would be warm.
dropped() {
    if sh -c "$drop"; then
        return 0
    fi
    echo "(the caches could not be dropped: this cold pass is skipped)"
    return 1
}

# Whether the folder is on a solid-state or a spinning disk.
disk() {
    local device rota
    device=$(findmnt -no SOURCE --target "$root" 2>/dev/null | sed 's/\[.*//' || true)
    rota=$(lsblk -dno ROTA "$device" 2>/dev/null | head -n1 | tr -d ' ' || true)
    case $rota in
    0) echo "solid state" ;;
    1) echo "spinning" ;;
    *) echo "unknown" ;;
    esac
}

{
    section "machine"
    changed=$(git -C "$here" status --porcelain 2>/dev/null || true)
    echo "skwird: $(git -C "$here" rev-parse --short HEAD 2>/dev/null || echo unknown)${changed:+ (changed)}"
    echo "kernel: $(uname -r)"
    echo "threads: $(nproc)"
    echo "memory: $(awk '/^MemTotal/ { printf "%.0f GB", $2 / 1e6 }' /proc/meminfo)"
    echo "disk: $(disk)"
    echo "load before: $(cut -d' ' -f1-3 /proc/loadavg)"

    if want walk; then
        if [ "$use_root" = 1 ]; then
            section "walk, names only, cold"
            if dropped; then run "$bench" walk "$root"; fi
        fi
        section "walk, names only, warm"
        run "$bench" walk "$root"
        if [ "$use_root" = 1 ]; then
            section "walk, every entry stat-ed, cold"
            if dropped; then run "$bench" walk "$root" --stat; fi
        fi
        section "walk, every entry stat-ed, warm"
        run "$bench" walk "$root" --stat
    fi

    if want check; then
        section "check of every folder's change time"
        if [ "$use_root" = 1 ]; then
            run "$bench" check "$root" --runs 2 --drop-with "$drop"
        else
            run "$bench" check "$root" --runs 2
        fi
    fi

    if want fanotify; then
        section "fanotify"
        if [ "$use_root" = 1 ]; then
            probes=(--probe "$probe")
            legend="1 = a folder in your cache"
            n=1
            for dir in /var/tmp /tmp; do
                if [ -d "$dir" ]; then
                    n=$((n + 1))
                    probes+=(--probe "$dir")
                    legend="$legend, $n = $dir"
                fi
            done
            echo "probes: $legend"
            run sudo -n "$bench" fanotify "$root" --seconds 2 "${probes[@]}"
        else
            echo "not tried: it needs sudo"
        fi
    fi

    echo
    echo "load after: $(cut -d' ' -f1-3 /proc/loadavg)"
} 2>&1 | tee "$out"

echo
echo "Saved in $out"
