#!/usr/bin/env bash
# Write side of the AUR-pkgver invariant (the read side is check-pkgver-sync.sh).
# Stamps the daemon crate's CURRENT version into every AUR PKGBUILD's `pkgver=`
# and resets `pkgrel=1` (Arch convention on a version change). release.yml's
# bump-version job runs this BEFORE the post-release daemon bump — i.e. while
# Cargo.toml still holds the version just tagged — so the PKGBUILDs pin the
# just-released tag (the one `source=` fetches), NOT the next unreleased dev
# version the bump moves the daemon to. Without it the PKGBUILDs would stay on
# the previous release and fail later packaging runs until a human edited all
# three.
#
#   sync-pkgver.sh
set -euo pipefail

# Read the version being released (the pre-bump daemon version). Same read as
# check-pkgver-sync.sh's bootstrap branch — keep these in step. awk (not
# `grep | head -1`) is SIGPIPE-free under `set -o pipefail`.
ver="$(awk -F'"' '/^version/{print $2; exit}' splitway-daemon/Cargo.toml)"
[ -n "$ver" ] || { echo "ERROR: could not read daemon version" >&2; exit 1; }

for pb in packaging/aur/*/PKGBUILD; do
    sed -i "s/^pkgver=.*/pkgver=$ver/" "$pb"
    sed -i "s/^pkgrel=.*/pkgrel=1/" "$pb"
    echo "synced $pb -> pkgver=$ver pkgrel=1"
done
