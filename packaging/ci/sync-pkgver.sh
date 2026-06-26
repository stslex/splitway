#!/usr/bin/env bash
# Write side of the AUR-pkgver invariant (the read side is check-pkgver-sync.sh).
# Stamps the daemon crate's CURRENT version into the SOURCE AUR PKGBUILDs'
# `pkgver=` and resets `pkgrel=1` (Arch convention on a version change).
# release.yml's bump-version job runs this BEFORE the post-release daemon bump —
# i.e. while Cargo.toml still holds the version just tagged — so the PKGBUILDs
# pin the just-released tag (the one `source=` fetches), NOT the next unreleased
# dev version the bump moves the daemon to. Without it the PKGBUILDs would stay
# on the previous release and fail later packaging runs until a human edited them.
#
# splitway-bin is EXCLUDED on purpose. The source PKGBUILDs fetch the `v$pkgver`
# TAG ARCHIVE, which GitHub generates the instant the tag exists, so pinning them
# the moment we tag is safe. splitway-bin instead fetches the release ASSET
# tarballs (splitway-$pkgver-linux-*.tar.gz) that packaging.yml attaches LATER
# and independently — so bumping it here would point it at not-yet (or, on a
# failed upload, never-) uploaded assets while the tag already exists. Its pkgver
# and real sha256sums are stamped together by the (deferred) asset-aware AUR-push
# automation, which by necessity runs after the assets are published.
#
#   sync-pkgver.sh
set -euo pipefail

# Read the version being released (the pre-bump daemon version). Same read as
# check-pkgver-sync.sh's bootstrap branch — keep these in step. awk (not
# `grep | head -1`) is SIGPIPE-free under `set -o pipefail`.
ver="$(awk -F'"' '/^version/{print $2; exit}' splitway-daemon/Cargo.toml)"
[ -n "$ver" ] || { echo "ERROR: could not read daemon version" >&2; exit 1; }

for pb in packaging/aur/*/PKGBUILD; do
    # Skip the asset-fetching prebuilt package (see header) — it must not be
    # advanced until its release tarballs exist.
    case "$pb" in */splitway-bin/PKGBUILD) echo "skipped $pb (asset-gated; deferred)"; continue ;; esac
    sed -i "s/^pkgver=.*/pkgver=$ver/" "$pb"
    sed -i "s/^pkgrel=.*/pkgrel=1/" "$pb"
    echo "synced $pb -> pkgver=$ver pkgrel=1"
done
