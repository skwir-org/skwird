#!/usr/bin/env bash
# A release's draft tried on its own system (docs/DESIGN.md, decision 11),
# on one of GitHub's runners, a machine thrown away after its job
# (.github/workflows/try.yml): its program for this system, built on
# Linux, taken from the draft, checked against the draft's checksums, and
# run here.
#
#     tools/release/try-draft.sh TAG macos|windows
#
# What it tries: macOS' signature (codesign; Gatekeeper's verdict said,
# not judged: the program is not notarized); an engine of its own, its
# folders in a scratch folder, its root one of synthetic names, started,
# proven current, asked a query; and the engine started at login
# (`logon add`), then taken back. Each step says what it found; the first
# that fails stops it.
set -euo pipefail
tag="${1:?usage: try-draft.sh TAG macos|windows}"
system="${2:?usage: try-draft.sh TAG macos|windows}"
repo="${GITHUB_REPOSITORY:?the repository, as a workflow names it}"
# This system's asset, known by how its name ends: a draft made to try
# this workflow alone holds what `release.sh programs` built.
case "$system" in
macos) suffix="-macos-arm64.tar.gz" ;;
windows) suffix="-windows-x86_64.zip" ;;
*)
    echo "try-draft.sh: no system $system" >&2
    exit 2
    ;;
esac

said() {
    printf '%s: %s\n' "$1" "$2"
}

fail() {
    printf 'FAILED: %s\n' "$*" >&2
    exit 1
}

# A path as the program takes it: Windows' own on Windows.
native() {
    if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

scratch="${RUNNER_TEMP:?the temporary folder a runner gives}/skwird-try"
if command -v cygpath >/dev/null; then scratch="$(cygpath -u "$scratch")"; fi
rm -rf "$scratch"
mkdir -p "$scratch/draft"
cd "$scratch/draft"

# The draft's asset and its checksums, by the API: a draft is no tag's yet,
# and only those who may write see it.
assets="$(gh api "repos/$repo/releases?per_page=100" \
    --jq ".[] | select(.tag_name == \"$tag\") | .assets[] | \"\(.id) \(.name)\"")"
[ -n "$assets" ] || fail "no release $tag, or none with assets"
asset="$(printf '%s\n' "$assets" | awk -v s="$suffix" '{ n = $2 } substr(n, length(n) - length(s) + 1) == s { print n }')"
[ "$(printf '%s\n' "$asset" | grep -c .)" = 1 ] || fail "the release $tag holds no one asset ending $suffix: $assets"
for name in "$asset" SHA256SUMS; do
    asset_id="$(printf '%s\n' "$assets" | awk -v n="$name" '$2 == n { print $1 }')"
    [ -n "$asset_id" ] || fail "the release $tag holds no $name"
    gh api -H "Accept: application/octet-stream" "repos/$repo/releases/assets/$asset_id" >"$name"
done
grep " $asset\$" SHA256SUMS >mine.sum || fail "SHA256SUMS names no $asset"
if command -v sha256sum >/dev/null; then sha256sum -c mine.sum; else shasum -a 256 -c mine.sum; fi
said checksum "$asset is the draft's"
unpacked="$scratch/draft/${asset%"$suffix"}${suffix%.tar.gz}"
unpacked="${unpacked%.zip}"

if [ "$system" = macos ]; then
    tar -xzf "$asset"
    bin="$unpacked/skwird"
    codesign --verify --strict --verbose=2 "$bin" 2>&1 | sed 's/^/codesign: /'
    said gatekeeper "$(spctl --assess --type execute "$bin" 2>&1 || true)"
else
    # Windows' own unzipping: Git's tar is GNU's, which reads no zip.
    powershell -NoProfile -Command "Expand-Archive -LiteralPath '$(native "$scratch/draft/$asset")' -DestinationPath '$(native "$scratch/draft")'"
    bin="$unpacked/skwird.exe"
fi
[ -x "$bin" ] || fail "no program in $asset"

# An engine of the try's own: its folders, its socket or pipe, its root.
home="$scratch/home"
mkdir -p "$home/notes" "$scratch/run" "$scratch/cache" "$scratch/state" "$scratch/config" "$scratch/local"
printf 'x' >"$home/notes/meeting-notes.txt"
export SKWIRD_NO_START=1
if [ "$system" = macos ]; then
    export XDG_RUNTIME_DIR="$scratch/run" XDG_CACHE_HOME="$scratch/cache" \
        XDG_STATE_HOME="$scratch/state" XDG_CONFIG_HOME="$scratch/config"
else
    LOCALAPPDATA="$(native "$scratch/local")"
    SKWIRD_PIPE="\\\\.\\pipe\\skwird-try-${GITHUB_RUN_ID:-0}"
    export LOCALAPPDATA SKWIRD_PIPE
fi
root="$(native "$home")"
"$bin" serve --root "$root" --idle 60 >"$scratch/engine.out" 2>&1 &
engine=$!
trap 'kill "$engine" 2>/dev/null || true' EXIT
for _ in $(seq 1 100); do
    if "$bin" status >/dev/null 2>&1; then break; fi
    sleep 0.2
done
"$bin" status >/dev/null 2>&1 || {
    cat "$scratch/engine.out" >&2
    fail "the engine never answered"
}
said engine "answers"
"$bin" sync "$root" | sed 's/^/sync: /'
found="$("$bin" find meeting --in "$root")"
case "$found" in
*meeting-notes.txt*) said query "the file of synthetic name found" ;;
*) fail "the query found no meeting-notes.txt: $found" ;;
esac
kill "$engine"
wait "$engine" 2>/dev/null || true
trap - EXIT

# Started at login, then taken back.
"$bin" logon add | sed 's/^/logon add: /'
if [ "$system" = macos ]; then
    launchctl print "gui/$(id -u)/org.skwir.skwird" >/dev/null || fail "launchd holds no org.skwir.skwird"
    said launchd "holds org.skwir.skwird"
fi
"$bin" logon remove | sed 's/^/logon remove: /'
said tried "$asset, on $system"
