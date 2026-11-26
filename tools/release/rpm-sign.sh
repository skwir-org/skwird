#!/bin/sh
# The release's `.rpm` signed by skwir's repositories' key (docs/PLAN.md,
# 0.3.0, step 6), as skwir's packages are (its tools/package/rpm-sign.sh,
# whose way this is), so that skwir's dnf repository lists it, which
# checks each (`gpgcheck=1`). Signed by GnuPG's keyring, whose agent asks
# for the key's passphrase: the secret part stays with whoever publishes,
# never in a repository. The key is named by its public file, which holds
# it alone (skwir's tools/package/repository/skwir.asc): what systems
# hold. A signature another key made goes first, so that each file ends
# signed by this key alone; each is then read back with that file alone,
# and one that does not say so stops the run.
#
#     tools/release/rpm-sign.sh KEY.asc [--at SECONDS] FILE...
#
# `--at` dates the signatures (seconds since the epoch, or the key's own
# making where that is later: gpg never signs before it): an EdDSA
# signature is then the same each time, and a package signed again stays
# the same file byte for byte. Needs rpmsign and rpmkeys (the flake's
# `release` shell) and gpg.
set -eu
usage() {
    echo "usage: $0 KEY.asc [--at SECONDS] FILE..." >&2
    exit 2
}
[ $# -ge 2 ] || usage
public="$1"
shift
at=""
if [ "$1" = --at ]; then
    [ $# -ge 3 ] || usage
    at="$2"
    shift 2
fi
for tool in rpmsign rpmkeys gpg; do
    command -v "$tool" >/dev/null || { echo "$0: $tool is needed" >&2; exit 1; }
done
[ -f "$public" ] || { echo "$0: $public: no such key file" >&2; exit 1; }
# The file's one key, by its primary fingerprint.
listing="$(gpg --batch --with-colons --show-keys "$public")"
[ "$(echo "$listing" | grep -c '^pub:')" = 1 ] || { echo "$0: $public: not one key" >&2; exit 1; }
key="$(echo "$listing" | awk -F: '$1 == "fpr" { print $10; exit }')"
gpg --batch --list-secret-keys "$key" >/dev/null 2>&1 ||
    { echo "$0: $key: its secret part is not in GnuPG's keyring" >&2; exit 1; }
gpg_program="$(command -v gpg)"
extra=""
if [ -n "$at" ]; then
    # The key's newest part (it or a subkey that signs) was made then.
    made="$(echo "$listing" | awk -F: '$1 == "pub" || $1 == "sub" { if ($6 > m) m = $6 } END { print m + 0 }')"
    [ "$at" -ge "$made" ] || at="$made"
    extra="--faked-system-time $at!"
fi

# The key's file alone, as the files are read back.
keyring="$(mktemp -d)"
trap 'rm -rf "$keyring"' EXIT INT TERM
rpmkeys --dbpath "$keyring" --import "$public"

for file in "$@"; do
    rpmsign --delsign "$file" >/dev/null
    rpmsign --define "_gpg_name $key" --define "__gpg $gpg_program" \
        ${extra:+--define "_gpg_sign_cmd_extra_args $extra"} --addsign "$file" >/dev/null
    said="$(LC_ALL=C rpmkeys --dbpath "$keyring" --checksig "$file")"
    case "$said" in
    *" signatures OK") echo "signed: $file, by $key" ;;
    *)
        echo "$0: $file: not signed by $key: $said" >&2
        exit 1
        ;;
    esac
done
