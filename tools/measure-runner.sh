#!/usr/bin/env bash
# skwird measured on one of GitHub's runners (docs/PLAN.md, "Measured on
# the runners"; .github/workflows/measure.yml): a stand-in for the
# measurement on a machine of the user's (phases 4 and 5, step 8), never
# that measurement. A runner is a shared virtual machine, and the tree is
# synthetic: orders of magnitude, and what must not move (idle at zero).
#
#     tools/measure-runner.sh BIN REPORT [ENTRIES]
#
# BIN holds skwird and skwird-bench, built for release. On a synthetic
# tree of ENTRIES (1,040,000 by default, phase 1's): the tree made; a walk
# (skwird-bench walk); an index built, saved, loaded and queried
# (skwird-bench index); then an engine of its own, its folders in a
# scratch folder: the time to its first answer and to live, a sync's wait
# after a change, its memory, and the processor it takes over ten idle
# minutes. What it writes in REPORT is counts, sizes and times alone.
set -euo pipefail
bin="${1:?usage: measure-runner.sh BIN REPORT [ENTRIES]}"
report="${2:?usage: measure-runner.sh BIN REPORT [ENTRIES]}"
entries="${3:-1040000}"
exe=""
case "$(uname -s)" in
Linux) system=linux ;;
Darwin) system=macos ;;
MINGW* | MSYS* | CYGWIN*)
    system=windows
    exe=.exe
    ;;
*)
    echo "measure-runner.sh: no system $(uname -s)" >&2
    exit 2
    ;;
esac
skwird="$bin/skwird$exe"
bench="$bin/skwird-bench$exe"

# A path as the programs take it: Windows' own on Windows.
native() {
    if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

# Seconds, to the millisecond: perl's, which every runner has (macOS'
# date gives no fraction).
now() {
    perl -MTime::HiRes=time -e 'printf "%.3f\n", time'
}

since() {
    awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.2f s", b - a }'
}

line() {
    printf '%s\n' "$*" | tee -a "$report"
}

scratch="${RUNNER_TEMP:?the temporary folder a runner gives}/skwird-measure"
if command -v cygpath >/dev/null; then scratch="$(cygpath -u "$scratch")"; fi
rm -rf "$scratch"
mkdir -p "$scratch"
: >"$report"

cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo "${NUMBER_OF_PROCESSORS:-?}")"
line "# skwird on GitHub's $system runner ($(uname -m), $cores cores): a stand-in"
line
tree="$scratch/tree"
line '```'
"$bench" tree "$(native "$tree")" --entries "$entries" | tee -a "$report"
line
"$bench" walk "$(native "$tree")" | tee -a "$report"
line
"$bench" index "$(native "$tree")" | tee -a "$report"
line '```'

# An engine of its own: its folders, its socket or pipe, its root.
mkdir -p "$scratch/run" "$scratch/cache" "$scratch/state" "$scratch/config" "$scratch/local"
export SKWIRD_NO_START=1
if [ "$system" = windows ]; then
    LOCALAPPDATA="$(native "$scratch/local")"
    SKWIRD_PIPE="\\\\.\\pipe\\skwird-measure-${GITHUB_RUN_ID:-0}"
    export LOCALAPPDATA SKWIRD_PIPE
else
    export XDG_RUNTIME_DIR="$scratch/run" XDG_CACHE_HOME="$scratch/cache" \
        XDG_STATE_HOME="$scratch/state" XDG_CONFIG_HOME="$scratch/config"
fi
root="$(native "$tree")"
begun="$(now)"
"$skwird" serve --root "$root" --forever >"$scratch/engine.out" 2>&1 &
engine=$!
trap 'kill "$engine" 2>/dev/null || true' EXIT
pid="$engine"
if [ "$system" = windows ]; then pid="$(cat "/proc/$engine/winpid")"; fi

# Its memory resident, in MB; its processor time, in seconds.
memory() {
    case "$system" in
    linux) awk '/^VmRSS:/ { printf "%.0f MB", $2 / 1000 }' "/proc/$pid/status" ;;
    macos) ps -o rss= -p "$pid" | awk '{ printf "%.0f MB", $1 * 1024 / 1e6 }' ;;
    windows) powershell -NoProfile -Command "[math]::Round((Get-Process -Id $pid).WorkingSet64 / 1e6)" | tr -d '\r' | sed 's/$/ MB/' ;;
    esac
}
processor() {
    case "$system" in
    linux) awk -v hz="$(getconf CLK_TCK)" '{ print ($14 + $15) / hz }' "/proc/$pid/stat" ;;
    # [[d-]h:]m:s, as ps prints it.
    macos) ps -o time= -p "$pid" | awk '{ n = split($1, p, /[-:]/); print p[n] + 60 * p[n - 1] + 3600 * (n > 2 ? p[n - 2] : 0) + 86400 * (n > 3 ? p[n - 3] : 0) }' ;;
    windows) powershell -NoProfile -Command "(Get-Process -Id $pid).TotalProcessorTime.TotalSeconds" | tr -d '\r' ;;
    esac
}

line
line "engine, on the tree:"
until "$skwird" coverage "$root" >/dev/null 2>&1; do
    if ! kill -0 "$engine" 2>/dev/null; then
        cat "$scratch/engine.out" >&2
        exit 1
    fi
    sleep 0.1
done
line "- first answer: $(since "$begun")"
# Kept live as a client keeps it (skwir's Kept live): on Linux, a user's
# engine keeps live what a client asks it to; macOS' and Windows' hear
# their roots whole.
"$skwird" watch "$root" >/dev/null
waited=0
until "$skwird" coverage "$root" 2>/dev/null | grep -q '^names: live'; do
    waited=$((waited + 1))
    if [ "$waited" -gt 9000 ]; then
        line "- live: NOT within 30 minutes: $("$skwird" coverage "$root" 2>&1 | head -1)"
        exit 1
    fi
    sleep 0.2
done
line "- live: $(since "$begun")"
line "- memory, live: $(memory)"
printf 'x' >"$tree/a change made now.txt"
asked="$(now)"
"$skwird" sync "$root" >/dev/null
line "- a sync after a change: $(since "$asked")"
"$skwird" find "change made now" --in "$root" | grep -q 'a change made now' ||
    { line "- the change: NOT FOUND"; exit 1; }
line "- the change: found"
# Ten minutes, or as long as SKWIRD_MEASURE_IDLE says (a try of the script).
idle="${SKWIRD_MEASURE_IDLE:-600}"
before="$(processor)"
sleep "$idle"
after="$(processor)"
line "- processor over $idle idle seconds: $(awk -v a="$before" -v b="$after" -v t="$idle" 'BEGIN { printf "%.2f s, %.3f%% of a core", b - a, 100 * (b - a) / t }')"
line "- memory, after: $(memory)"
kill "$engine"
wait "$engine" 2>/dev/null || true
trap - EXIT
rm -rf "$scratch"
