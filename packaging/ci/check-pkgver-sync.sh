#!/usr/bin/env bash
# Guard the AUR-pkgver invariant: the version pinned in the committed SOURCE
# PKGBUILDs must name a release that ACTUALLY EXISTS, because each `source=`
# fetches the `v$pkgver` git tag archive. release.yml's post-release auto-bump
# pushes splitway-daemon/Cargo.toml to the NEXT (unreleased) version, so the
# daemon version is deliberately one ahead of the latest release at rest on
# master — the PKGBUILDs must therefore pin a real *released tag*, never the
# in-tree daemon version (the write side, sync-pkgver.sh, stamps the version
# being released). Fail the build on drift so `makepkg -si` from a checkout can
# never point at a tag that does not exist.
#
# splitway-bin is EXCLUDED: it fetches release ASSET tarballs (attached later by
# packaging.yml), so tag-existence — all this script cheaply checks — is
# necessary but not sufficient for it, and it is allowed to lag the source
# pkgver. Its asset-gated pin + sha256sums are owned by the (deferred) asset-aware
# AUR-push automation (see splitway-bin/PKGBUILD and sync-pkgver.sh).
#
# Needs the tags fetched (packaging.yml's meta job uses fetch-depth: 0).
#
#   check-pkgver-sync.sh
set -euo pipefail

# The source PKGBUILDs describe one project at one version, so they must agree.
# awk (not `grep | head -1`) is SIGPIPE-free under `set -o pipefail`.
pkgver=""
for pb in packaging/aur/*/PKGBUILD; do
    # See header: splitway-bin's pkgver is asset-gated and may lag — not checked here.
    case "$pb" in */splitway-bin/PKGBUILD) continue ;; esac
    pv="$(awk -F= '/^pkgver=/{print $2; exit}' "$pb")"
    [ -n "$pv" ] || { echo "ERROR: $pb has no pkgver=" >&2; exit 1; }
    if [ -z "$pkgver" ]; then
        pkgver="$pv"
    elif [ "$pv" != "$pkgver" ]; then
        echo "ERROR: $pb has pkgver=$pv but the others use $pkgver (AUR PKGBUILDs must share one version)" >&2
        exit 1
    fi
done
[ -n "$pkgver" ] || { echo "ERROR: no PKGBUILDs found under packaging/aur/" >&2; exit 1; }

# Does the pinned release tag exist? `--count=1` makes git stop after the first
# match, so the captured read is SIGPIPE-free under `set -o pipefail`.
if [ -n "$(git for-each-ref --count=1 --format='%(refname:short)' "refs/tags/v$pkgver")" ]; then
    echo "pkgver $pkgver in sync — release tag v$pkgver exists"
    exit 0
fi

# No tag for this pkgver. If ANY v* tag exists we have drifted onto a
# non-existent release (the bug this gate guards). Only the pre-first-release
# bootstrap — no v* tags at all — is allowed, and then only while the pkgver
# still tracks the daemon version (the convention before the first release).
if [ -n "$(git for-each-ref --count=1 --format='%(refname:short)' 'refs/tags/v*')" ]; then
    echo "ERROR: PKGBUILD pkgver=$pkgver but release tag v$pkgver does not exist (pin a released tag)" >&2
    exit 1
fi

ver="$(awk -F'"' '/^version/{print $2; exit}' splitway-daemon/Cargo.toml)"
if [ "$pkgver" = "$ver" ]; then
    echo "pkgver $pkgver matches daemon version — no release tagged yet (bootstrap)"
    exit 0
fi
echo "ERROR: no release tag exists yet and pkgver=$pkgver differs from daemon version=$ver" >&2
exit 1
