#!/usr/bin/env bash
# skwird's whole measurement (docs/PLAN.md: phase 0, and phase 1's step
# 6) on one folder, your home by default, kept in one file you can share:
# it holds counts, sizes and times, never a name or a path.
#
#   tools/measure.sh [--no-root] [--only STEP[,STEP]...] [FOLDER]
#
# STEP: walk, check, fanotify, index, engine, live, system. Every step by
# default but system, which runs only when named: it runs the system
# service (phase 3) as root over the whole machine, its folders in your
# cache, and asks it as you, then removes what it built.
#
# It asks for your password once (sudo), to drop the caches before each
# cold pass (the machine is slower for a while after) and to try
# fanotify, which only root may use on a whole filesystem. On a btrfs
# subvolume, fanotify also mounts the filesystem's top-level subvolume,
# read-only and attached nowhere, for the seconds it listens. --no-root
# skips both: warm passes only, no fanotify. --only runs the steps it
# names.
#
# The index and the engine steps build skwird's index of the folder in
# a folder of their own in your cache, and remove it once measured. The
# live step runs an engine keeping folders live (phase 2): the time from
# a change to its answer, in a folder of its own in your cache, then the
# folder itself watched as far as the engine's share of watches goes. It
# takes a few minutes; an idle machine gives the fairest numbers.
set -euo pipefail

usage='tools/measure.sh [--no-root] [--only STEP[,STEP]...] [FOLDER]'
use_root=1
only=
folder=$HOME
while [ $# -gt 0 ]; do
    case $1 in
    --no-root) use_root=0 ;;
    --only)
        # One step, or several with commas between: engine,live.
        for step in ${2//,/ }; do
            case $step in
            walk | check | fanotify | index | engine | live | system) ;;
            *)
                echo "--only takes walk, check, fanotify, index, engine, live or system, or several with commas ($usage)" >&2
                exit 2
                ;;
            esac
        done
        only=${2:-}
        shift
        ;;
    -h | --help)
        sed -n '2,/^set -euo/{/^#/s/^# \{0,1\}//p}' "$0"
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
# Whether a step runs: every step, or those --only names.
want() { [ -z "$only" ] || [[ ",$only," == *",$1,"* ]]; }
if [ "$(id -u)" = 0 ]; then
    # As root, the home folder and the cache would be root's.
    echo "Run it as yourself: it asks for sudo when it needs it." >&2
    exit 2
fi
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
root=$(realpath "$folder")
bench=$here/target/release/skwird-bench
engine=$here/target/release/skwird
cache=${XDG_CACHE_HOME:-$HOME/.cache}/skwird-bench
out=$cache/measured-$(date +%Y%m%d-%H%M%S).txt
probe=$cache/probe

if command -v cargo >/dev/null 2>&1 &&
    { [ ! -x "$bench" ] || [ ! -x "$engine" ] ||
        [ -n "$(find "$here/crates" "$here/Cargo.lock" -newer "$bench" -print -quit)" ]; }; then
    echo "Building skwird-bench and skwird…"
    if ! (cd "$here" && cargo build --release --quiet -p skwird-bench -p skwird); then
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
engine_pid=
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
    # The engine and what the index and engine steps built, if one was
    # cut short.
    if [ -n "$engine_pid" ]; then kill "$engine_pid" 2>/dev/null || true; fi
    rm -rf "$cache/index" "$cache/engine"
    if [ -n "${sys_pid:-}" ]; then sudo -n kill "$sys_pid" 2>/dev/null || true; fi
    if [ -d "${sys_dir:-}" ]; then sudo -n rm -rf "$sys_dir" 2>/dev/null || true; fi
}
trap cleanup EXIT

drop='sync; echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null'

# The engine, with runtime, cache and state folders of its own in the
# measurement's cache: it never meets your own engine, and what it builds
# is removed once measured. HOME stays yours, so that the default
# exclusions (~/.ssh, ~/.gnupg, the keyrings) are your own folders.
engine_dir=$cache/engine
skw() {
    env XDG_RUNTIME_DIR="$engine_dir/run" XDG_CACHE_HOME="$engine_dir/cache" \
        XDG_STATE_HOME="$engine_dir/state" SKWIRD_NO_START=1 "$engine" "$@"
}

# The engine, in the background: through `env`, which becomes the program,
# so that `$!` is the engine's own pid. A function run with `&` would be a
# subshell, and its pid not the engine's. Run as its unit runs it: at a
# niceness of 10, its I/O idle (`Nice=10`, `IOSchedulingClass=idle`), so
# that it yields to the machine's own work as it would.
serve_engine() {
    env XDG_RUNTIME_DIR="$engine_dir/run" XDG_CACHE_HOME="$engine_dir/cache" \
        XDG_STATE_HOME="$engine_dir/state" nice -n 10 ionice -c 3 \
        "$engine" serve --root "$root" --idle 600 2>>"$engine_dir/engine.log" &
    engine_pid=$!
}

now() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.2f", b - a }'; }

# Wait until the engine answers with its names checked or as of a time:
# its index built or read back. Gives up after ten minutes.
ready() {
    local deadline=$((SECONDS + 600))
    while [ $SECONDS -lt $deadline ]; do
        if [ -S "$engine_dir/run/skwird/socket" ] &&
            skw coverage "$root" 2>/dev/null | grep -Eq '^names: (checked|as of)'; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

# A process's processor time so far, in seconds.
cpu() { awk -v hz="$(getconf CLK_TCK)" '{ printf "%.2f", ($14 + $15) / hz }' "/proc/$1/stat"; }

# What a process's status on stdin says of its memory: resident now, of it
# its own (the allocator's) and its files' mapped (the index's, the page
# cache's to drop), and at most.
status_memory() {
    awk '/^VmRSS/ { now = $2 } /^VmHWM/ { most = $2 } /^RssAnon/ { own = $2 } /^RssFile/ { mapped = $2 }
        END { printf "%.0f MB now (%.0f MB its own, %.0f MB of files mapped), %.0f MB at most", now / 1000, own / 1000, mapped / 1000, most / 1000 }'
}

memory() {
    status_memory < "/proc/$1/status"
}

measure_engine() {
    rm -rf "$engine_dir"
    mkdir -p "$engine_dir/run" "$engine_dir/cache" "$engine_dir/state"
    local start pid
    start=$(now)
    serve_engine
    pid=$engine_pid
    if ! ready; then
        echo "(the engine never answered)"
        kill "$pid" 2>/dev/null || true
        return 1
    fi
    echo "first answer, the index built: $(since "$start") s"
    echo "memory once built: $(memory "$pid")"
    start=$(now)
    skw sync "$root" >/dev/null
    echo "a sync (every folder checked), warm: $(since "$start") s"
    local before
    before=$(cpu "$pid")
    sleep 30
    echo "idle, 30 s without a client: $(awk -v a="$before" -v b="$(cpu "$pid")" 'BEGIN { printf "%.2f", b - a }') s of processor time"
    echo "memory idle: $(memory "$pid")"
    kill "$pid"
    wait "$pid" 2>/dev/null || true
    start=$(now)
    serve_engine
    pid=$engine_pid
    if ready; then
        echo "first answer, the index read back: $(since "$start") s"
        echo "memory once read back: $(memory "$pid")"
    else
        echo "(the engine never answered, read back)"
    fi
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "the engine's log (counts and times):"
    sed 's/^/  /' "$engine_dir/engine.log"
    echo "index on disk: $(du -sb "$engine_dir/cache" | awk '{ printf "%.1f MB", $1 / 1e6 }')"
    rm -rf "$engine_dir"
}

# Milliseconds since a time `now` gave.
since_ms() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%d", (b - a) * 1000 }'; }

# The watches a process holds, from its inotify descriptors' fdinfo.
watches() {
    local n=0 fd
    for fd in /proc/"$1"/fd/*; do
        if [ "$(readlink "$fd" 2>/dev/null)" = "anon_inode:inotify" ]; then
            n=$((n + $(grep -c '^inotify wd:' "/proc/$1/fdinfo/${fd##*/}" 2>/dev/null || echo 0)))
        fi
    done
    echo "$n"
}

# The engine with folders kept live (phase 2): the time from a change in
# a small watched folder to its answer, then the folder measured itself
# watched, as far as the engine's share of watches goes (the most a
# user's engine holds), and what that costs.
measure_live() {
    rm -rf "$engine_dir"
    mkdir -p "$engine_dir/run" "$engine_dir/cache" "$engine_dir/state"
    local start pid before
    serve_engine
    pid=$engine_pid
    if ! ready; then
        echo "(the engine never answered)"
        kill "$pid" 2>/dev/null || true
        return 1
    fi
    local probe=$cache/live-probe
    case $probe in
    "$root"/*)
        rm -rf "$probe"
        mkdir -p "$probe"
        skw watch "$probe" >/dev/null
        local times=() i t0
        for i in $(seq 1 20); do
            t0=$(now)
            : >"$probe/p$i"
            local deadline=$((SECONDS + 10))
            until skw find "p$i" --in "$probe" 2>/dev/null | grep -q .; do
                if [ $SECONDS -ge $deadline ]; then
                    echo "(a change was not answered within 10 s)"
                    break 2
                fi
                sleep 0.005
            done
            times+=("$(since_ms "$t0")")
        done
        if [ ${#times[@]} -eq 20 ]; then
            printf '%s\n' "${times[@]}" | sort -n | awk '{ a[NR] = $1 }
                END { printf "a change to its answer, live (20 files made): median %d ms, most %d ms\n", a[int((NR + 1) / 2)], a[NR] }'
        fi
        : >"$probe/before-sync"
        start=$(now)
        skw sync "$probe" >/dev/null
        echo "a sync of a live folder, a change made just before: $(since_ms "$start") ms ($(skw coverage "$probe" | sed -n 's/^names: \([a-z]*\).*/\1/p'))"
        skw watch --remove "$probe" >/dev/null
        rm -rf "$probe"
        ;;
    *)
        echo "a change to its answer: not measured, the folder does not hold the cache"
        ;;
    esac
    start=$(now)
    skw watch "$root" >/dev/null
    echo "the folder watched whole, as far as the share goes: $(since "$start") s"
    echo "watches held: $(watches "$pid") (the kernel's memory, about 1 KB each)"
    echo "memory watching: $(memory "$pid")"
    start=$(now)
    skw sync "$root" >/dev/null
    echo "a sync of it, $(skw coverage "$root" | sed -n 's/^names: \([a-z]*\).*/\1/p'): $(since "$start") s"
    # Past the save due 10 s after the last change heard: idle, then.
    sleep 11
    before=$(cpu "$pid")
    sleep 30
    echo "idle, 30 s watching: $(awk -v a="$before" -v b="$(cpu "$pid")" 'BEGIN { printf "%.2f", b - a }') s of processor time"
    echo "memory idle watching: $(memory "$pid")"
    kill "$pid"
    wait "$pid" 2>/dev/null || true
    echo "the engine's log (counts and times):"
    sed 's/^/  /' "$engine_dir/engine.log"
    rm -rf "$engine_dir"
}

# The system service (phase 3), as root: every local filesystem that
# stores files marked whole and indexed, its folders in the measurement's
# cache, its socket where a user's client looks for its own, so that it
# never meets an engine of yours. Asked as you: the time to its first live
# answer, a change answered, a sync, your queries filtered for you, idle,
# memory; then read back. Never run unless named (--only system).
sys_dir=$cache/system
sys_pid=
sys_sudo=
sysw() {
    env XDG_RUNTIME_DIR="$sys_dir/run" SKWIRD_NO_START=1 "$engine" "$@"
}
# The system service, as its unit runs it: at a niceness of 10, its I/O
# idle.
serve_system() {
    sudo -n env RUNTIME_DIRECTORY="$sys_dir/run/skwird" CACHE_DIRECTORY="$sys_dir/cache" \
        STATE_DIRECTORY="$sys_dir/state" CONFIGURATION_DIRECTORY="$sys_dir/config" \
        nice -n 10 ionice -c 3 "$engine" serve --system --forever 2>>"$sys_dir/engine.log" &
    sys_sudo=$!
    sys_pid=
    # sudo's child, or, where sudo runs it through a pty, its grandchild.
    local parents c
    for _ in $(seq 1 100); do
        parents=$sys_sudo
        for _ in 1 2 3; do
            for c in $(pgrep -P "$parents" 2>/dev/null || true); do
                if [ "$(cat "/proc/$c/comm" 2>/dev/null)" = skwird ]; then sys_pid=$c; fi
            done
            [ -n "$sys_pid" ] && break 2
            parents=$(pgrep -P "$parents" 2>/dev/null | paste -sd, || true)
            [ -n "$parents" ] || break
        done
        sleep 0.05
    done
}
stop_system() {
    if [ -n "$sys_pid" ]; then sudo -n kill "$sys_pid" 2>/dev/null || true; fi
    if [ -n "$sys_sudo" ]; then wait "$sys_sudo" 2>/dev/null || true; fi
    sys_pid=
    sys_sudo=
}
live_answer() {
    local deadline=$((SECONDS + 1800))
    while [ $SECONDS -lt $deadline ]; do
        if sysw coverage "$HOME" 2>/dev/null | grep -q '^names: live since '; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}
# Wait until the service has a line matching `$2` for each root it served
# at its start, by its number, in its log from line `$1` on: each root
# built (or read back), or checked. A root mounted since is not counted.
# Gives up after half an hour.
every_root() {
    local deadline=$((SECONDS + 1800)) log n i missing
    while [ $SECONDS -lt $deadline ]; do
        log=$(tail -n +"$1" "$sys_dir/engine.log" 2>/dev/null)
        n=$(printf '%s\n' "$log" | sed -n 's/^skwird: serving \([0-9]*\) roots$/\1/p' | head -1)
        if [ -n "$n" ]; then
            missing=
            for i in $(seq 0 $((n - 1))); do
                printf '%s\n' "$log" | grep -qE "^skwird: root $i ($2)" || missing=$i
            done
            [ -z "$missing" ] && return 0
        fi
        sleep 0.2
    done
    return 1
}
# The service's log, which says counts and times, never a name or a path.
system_log() {
    echo "the service's log (counts and times):"
    sed 's/^/  /' "$sys_dir/engine.log"
}
measure_system() {
    if [ -S /run/skwird/socket ]; then
        echo "(a system service runs here: not measured, so as not to meet it)"
        return 0
    fi
    sudo -n rm -rf "$sys_dir"
    mkdir -p "$sys_dir"
    local start
    start=$(now)
    serve_system
    if [ -z "$sys_pid" ] || ! live_answer; then
        echo "(the service never answered live)"
        stop_system
        system_log
        return 1
    fi
    # Each root is opened on a thread of its own: the home is answered
    # live once its own roots are, a slow filesystem built after.
    echo "your home answered live: $(since "$start") s"
    if every_root 1 'built|loaded|cannot be opened'; then
        echo "every filesystem built: $(since "$start") s"
    else
        echo "(not every filesystem built in half an hour)"
    fi
    echo "memory once built: $(sudo -n cat "/proc/$sys_pid/status" | status_memory)"
    local times=() i t0 name late=0
    for i in $(seq 1 10); do
        name="skwird-system-probe-$i"
        t0=$(now)
        : >"$probe/$name"
        until sysw find "$name" --in "$probe" 2>/dev/null | grep -q .; do
            if [ "$(since "$t0" | cut -d. -f1)" -ge 10 ]; then
                late=$((late + 1))
                break
            fi
            sleep 0.005
        done
        times+=("$(since_ms "$t0")")
        rm -f "$probe/$name"
        sleep 0.3
    done
    # One not answered in 10 s is counted at 10 s, and said.
    printf '%s\n' "${times[@]}" | sort -n | awk -v late="$late" '{ a[NR] = $1 } END { printf "a change to its answer, live (%d files made): median %d ms, most %d ms", NR, a[int((NR + 1) / 2)], a[NR]; if (late > 0) printf " (%d not answered within 10 s, counted at 10 s)", late; printf "\n" }'
    start=$(now)
    sysw sync "$HOME" >/dev/null
    echo "a sync of your home (the barrier): $(since "$start") s"
    start=$(now)
    local n
    n=$(sysw find e --in "$HOME" --limit 100000 2>/dev/null | wc -l)
    echo "a query of your home as you, filtered for you: $(since "$start") s, $n results"
    start=$(now)
    n=$(sudo -n env XDG_RUNTIME_DIR="$sys_dir/run" SKWIRD_NO_START=1 "$engine" find e --in "$HOME" --limit 100000 2>/dev/null | wc -l)
    echo "the same as root, not filtered: $(since "$start") s, $n results"
    local before
    before=$(sudo -n awk '{ print $14 + $15 }' "/proc/$sys_pid/stat")
    sleep 30
    echo "idle, 30 s without a client: $(sudo -n awk -v a="$before" -v hz="$(getconf CLK_TCK)" '{ printf "%.2f", ($14 + $15 - a) / hz }' "/proc/$sys_pid/stat") s of processor time"
    echo "memory idle: $(sudo -n cat "/proc/$sys_pid/status" | status_memory)"
    stop_system
    local from
    from=$(($(wc -l <"$sys_dir/engine.log") + 1))
    start=$(now)
    serve_system
    if [ -n "$sys_pid" ] && live_answer; then
        echo "your home answered live, read back and checked: $(since "$start") s"
        if every_root "$from" 'checked in|cannot be checked|cannot be opened'; then
            echo "every filesystem read back and checked: $(since "$start") s"
        fi
    else
        echo "(the service never answered live, read back)"
    fi
    stop_system
    system_log
    echo "index on disk: $(sudo -n du -sb "$sys_dir/cache" | awk '{ printf "%.1f MB", $1 / 1e6 }')"
    sudo -n rm -rf "$sys_dir"
}

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

    if want index; then
        section "index: built, saved and read back (skwird-bench)"
        run "$bench" index "$root" --keep "$cache/index"
        rm -rf "$cache/index"
    fi

    if want engine; then
        section "engine: a user's engine on the folder"
        run measure_engine
    fi

    if want live; then
        section "live: folders kept live (phase 2)"
        run measure_live
    fi

    # Named alone: it runs the system service as root over the machine.
    if [[ ",$only," == *",system,"* ]]; then
        section "system: the system service (phase 3), as root"
        if [ "$use_root" = 1 ]; then
            run measure_system
        else
            echo "not tried: it needs sudo"
        fi
    fi

    echo
    echo "load after: $(cut -d' ' -f1-3 /proc/loadavg)"
} 2>&1 | tee "$out"

echo
echo "Saved in $out"
