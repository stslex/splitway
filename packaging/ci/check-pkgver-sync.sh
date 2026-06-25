#!/usr/bin/env bash
# Guard the lockstep-version invariant. The single source of truth is the daemon
# crate's version (splitway-daemon/Cargo.toml, read by compute-version.sh); the
# deb/rpm packages are stamped from it in CI, but the AUR PKGBUILDs carry a
# hand-kept `pkgver=` that is easy to forget on a version bump. Fail the build if
# any PKGBUILD's pkgver has drifted from the daemon version, so the three can't
# silently diverge.
#
#   check-pkgver-sync.sh
set -euo pipefail

# awk (not `grep | head -1`): SIGPIPE-free under `set -o pipefail` if a second
# `^version` line ever appears. Mirrors compute-version.sh / sync-pkgver.sh.
ver="$(awk -F'"' '/^version/{print $2; exit}' splitway-daemon/Cargo.toml)"
[ -n "$ver" ] || { echo "ERROR: could not read daemon version" >&2; exit 1; }

rc=0
for pb in packaging/aur/*/PKGBUILD; do
    pv="$(awk -F= '/^pkgver=/{print $2; exit}' "$pb")"
    if [ "$pv" != "$ver" ]; then
        echo "ERROR: $pb has pkgver=$pv but daemon version=$ver (lockstep drift)" >&2
        rc=1
    fi
done

if [ "$rc" = 0 ]; then
    echo "pkgver in sync ($ver) across all AUR PKGBUILDs"
fi
exit "$rc"
