#!/usr/bin/env bash
# Write side of the lockstep-version invariant (the read side is
# check-pkgver-sync.sh). The daemon crate's version is the single source of
# truth; this stamps it into every AUR PKGBUILD's `pkgver=` and resets `pkgrel=1`
# (Arch convention on a version change). Run it right after bumping
# splitway-daemon/Cargo.toml so the gate in packaging.yml's `meta` job stays
# green — without this, each post-release auto-bump would leave the PKGBUILDs
# behind and fail every later packaging run until a human edited all three.
#
#   sync-pkgver.sh
set -euo pipefail

# Same read as check-pkgver-sync.sh — keep these two in step.
ver="$(grep '^version' splitway-daemon/Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')"
[ -n "$ver" ] || { echo "ERROR: could not read daemon version" >&2; exit 1; }

for pb in packaging/aur/*/PKGBUILD; do
    sed -i "s/^pkgver=.*/pkgver=$ver/" "$pb"
    sed -i "s/^pkgrel=.*/pkgrel=1/" "$pb"
    echo "synced $pb -> pkgver=$ver pkgrel=1"
done
