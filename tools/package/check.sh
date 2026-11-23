#!/bin/sh
# What an installed package of skwird must hold (docs/DESIGN.md,
# decision 11), checked on the system it was installed on, a root
# tools/package/try.sh made:
#
#     tools/package/check.sh installed    # as root, once installed
#     tools/package/check.sh engine       # as a user, once installed
#     tools/package/check.sh removed      # as root, once removed
#
# - installed: the program, where each unit runs it; the user's units and
#   the system service's, the system service's user (made by the
#   package's scripts or the package manager's hooks), the licences; and
#   nothing enabled.
# - engine: a user's engine, started by its first client as a package's
#   user would, indexes a home of synthetic names (spaces, Unicode, a
#   link), answers them, and never a private folder's.
# - removed: no file of the package's left, nor the system service's
#   index.
set -eu
failed=0
fail() {
    echo "FAIL: $*" >&2
    failed=1
}
files="/usr/bin/skwird
/usr/lib/systemd/user/skwird.service
/usr/lib/systemd/user/skwird.socket
/usr/lib/systemd/system/skwird-system.service
/usr/lib/sysusers.d/skwird.conf"
case "${1:-}" in
installed)
    for f in $files; do
        [ -f "$f" ] || fail "$f is not installed"
    done
    [ -x /usr/bin/skwird ] || fail "/usr/bin/skwird does not run"
    # Each unit runs the program installed.
    for unit in /usr/lib/systemd/user/skwird.service /usr/lib/systemd/system/skwird-system.service; do
        program="$(sed -n 's/^ExecStart=\([^ ]*\).*/\1/p' "$unit")"
        [ -x "$program" ] || fail "$unit runs $program, which is not installed"
    done
    # The licences, where the system keeps them.
    for licence in LICENSE-MIT LICENSE-APACHE; do
        [ -f "/usr/share/doc/skwird/$licence" ] || [ -f "/usr/share/licenses/skwird/$licence" ] ||
            fail "no $licence"
    done
    # The system service's user, with no login.
    if entry="$(getent passwd skwird)"; then
        case "$entry" in
        *:/var/lib/skwird:*nologin | *:/var/lib/skwird:/bin/false) ;;
        *) fail "the skwird user is not the service's: $entry" ;;
        esac
    else
        fail "no skwird user"
    fi
    # Nothing enabled: no link to a unit of skwird's, for the system or a
    # user.
    enabled="$(find /etc/systemd -name 'skwird*' 2>/dev/null || true)"
    [ -z "$enabled" ] || fail "enabled at installation: $enabled"
    ;;
engine)
    # A home of the user's own, its folders the engine's (never this
    # system's user's: the root is a test's).
    export HOME="$HOME/check"
    rm -rf "$HOME"
    run="$(mktemp -d)"
    export XDG_RUNTIME_DIR="$run" XDG_CACHE_HOME="$HOME/.cache" XDG_STATE_HOME="$HOME/.local/state" XDG_CONFIG_HOME="$HOME/.config"
    # The engine goes a second after its last client.
    export SKWIRD_IDLE=1
    dir="$HOME/Projets/été 2026/a b"
    mkdir -p "$dir" "$HOME/.ssh"
    : > "$dir/notes 1.txt"
    : > "$HOME/.ssh/id_check"
    ln -s "notes 1.txt" "$dir/lien vers notes"
    said="$(skwird sync "$HOME" 2>&1)" || fail "skwird sync: $said"
    case "$said" in
    *"checked at"* | *"live since"*) ;;
    *) fail "a sync of the home proves it current: $said" ;;
    esac
    found="$(skwird find notes --in "$HOME" 2>&1)" || fail "skwird find: $found"
    for path in "$dir/notes 1.txt" "$dir/lien vers notes"; do
        printf '%s\n' "$found" | grep -qxF "$path" || fail "find notes answers $path: $found"
    done
    printf '%s\n' "$found" | grep -q '^2 found' || fail "find notes answers two: $found"
    private="$(skwird find id_check --in "$HOME" 2>&1)" || fail "skwird find: $private"
    printf '%s\n' "$private" | grep -q '^0 found' || fail "a private folder's name is answered: $private"
    status="$(skwird status 2>&1)" || fail "skwird status: $status"
    printf '%s\n' "$status" | grep -q "^root: $HOME$" || fail "the engine's root is the home: $status"
    rm -rf "$run"
    ;;
removed)
    for f in $files /usr/share/doc/skwird/LICENSE-MIT /usr/share/licenses/skwird/LICENSE-MIT; do
        [ ! -e "$f" ] || fail "$f is left"
    done
    [ ! -e /var/cache/skwird ] || fail "the system service's index is left"
    ;;
*)
    echo "usage: $0 installed|engine|removed" >&2
    exit 2
    ;;
esac
if [ "$failed" = 0 ]; then
    echo "== $1: held"
fi
exit "$failed"
