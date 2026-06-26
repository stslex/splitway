#!/usr/bin/env bash
# Derive the package version + channel for a CI run from the daemon's Cargo.toml
# version (the single source of truth) and the triggering event. Prints
# `key=value` lines suitable for appending to $GITHUB_OUTPUT.
#
#   compute-version.sh <event_name> <ref>
#
# Run ONCE (in the meta job) and fan the outputs out to the other jobs, so the
# timestamp/sha are identical everywhere.
#
# Release channel: push to master -> clean <X.Y.Z>.
# Dev channel: everything else (push to dev, pull_request, workflow_dispatch) ->
#   <X.Y.Z>~dev.<utcYYYYmmddHHMMSS>.<runNumber>.<shortsha>. The `~dev` suffix
# sorts BELOW the clean release in both dpkg and rpm (>=4.10), so a tester with
# both repos enabled upgrades dev -> release cleanly. <runNumber> keeps two
# same-second builds monotonically ordered (see below).
set -euo pipefail

event="${1:?usage: compute-version.sh <event_name> <ref>}"
ref="${2:?usage: compute-version.sh <event_name> <ref>}"

# awk (not `grep | head -1`): under `set -o pipefail` head closing the pipe
# early would SIGPIPE grep and fail the read if a second `^version` line ever
# appeared; awk exits cleanly on the first match.
version="$(awk -F'"' '/^version/{print $2; exit}' splitway-daemon/Cargo.toml)"

if [ "$event" = "push" ] && [ "$ref" = "refs/heads/master" ]; then
    pkgver="$version"
    channel="release"
else
    utc="$(date -u +%Y%m%d%H%M%S)"
    sha="$(git rev-parse --short HEAD)"
    # GITHUB_RUN_NUMBER increments per run, so two pushes that land in the same
    # UTC second still order monotonically — without it dpkg/rpm fall back to
    # comparing the short sha lexically, which is NOT monotonic with commit order
    # and can pick the wrong "newest" build. Defaults to 0 outside Actions.
    run="${GITHUB_RUN_NUMBER:-0}"
    pkgver="${version}~dev.${utc}.${run}.${sha}"
    channel="dev"
fi

echo "version=$version"
echo "pkgver=$pkgver"
echo "channel=$channel"
