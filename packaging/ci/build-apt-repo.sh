#!/usr/bin/env bash
# Build (and, given a key, GPG-sign) an apt repository for ONE channel.
#
#   build-apt-repo.sh <repo-root> [gpg-key-id]
#
# <repo-root> is e.g. gh-pages/deb/release. The caller must have already placed
# the channel's .deb files under <repo-root>/pool/main/ (this script never
# deletes existing packages — old versions persist, matching the "merge, never
# wipe" Pages policy). It (re)generates the per-arch Packages indexes and the
# suite Release file from whatever is in the pool. If a gpg-key-id is given,
# it writes a clearsigned InRelease and a detached Release.gpg.
#
# Used by BOTH the PR signed-repo test (ephemeral key, packaging.yml) and the
# real publish job (real key) — identical metadata, different signer.
set -euo pipefail

ROOT="${1:?usage: build-apt-repo.sh <repo-root> [gpg-key-id]}"
KEY="${2:-}"

SUITE="stable"
COMP="main"
ARCHES="amd64 arm64"

# Sign with the given key, non-interactively. If SPLITWAY_GPG_PASSFILE points at
# a passphrase file (the real key has a passphrase), feed it via loopback; the
# ephemeral CI key has none, so the var is unset and no passphrase is needed.
gpg_sign() {
    local extra=()
    [ -n "${SPLITWAY_GPG_PASSFILE:-}" ] && extra=(--passphrase-file "$SPLITWAY_GPG_PASSFILE")
    gpg --batch --yes --pinentry-mode loopback "${extra[@]}" --default-key "$KEY" "$@"
}

[ -d "$ROOT/pool/main" ] || { echo "error: $ROOT/pool/main does not exist (no packages)" >&2; exit 1; }
cd "$ROOT"

# Split a combined Packages index into one stanza-set per architecture (apt
# fetches dists/$SUITE/$COMP/binary-<arch>/Packages and wants only that arch +
# 'all'). RS="" => paragraph (stanza) mode.
filter_arch() { # <arch>  (reads a Packages file on stdin)
    awk -v want="$1" 'BEGIN { RS = ""; ORS = "\n\n" }
        {
            arch = ""
            n = split($0, lines, "\n")
            for (i = 1; i <= n; i++)
                if (lines[i] ~ /^Architecture: /) { arch = lines[i]; sub(/^Architecture: /, "", arch); break }
            if (arch == want || arch == "all") print
        }'
}

allpkgs="$(mktemp)"
apt-ftparchive packages pool/main > "$allpkgs"

for a in $ARCHES; do
    dir="dists/$SUITE/$COMP/binary-$a"
    mkdir -p "$dir"
    filter_arch "$a" < "$allpkgs" > "$dir/Packages"
    gzip -9kf "$dir/Packages"
done
rm -f "$allpkgs"

# Suite Release file (with checksums of the Packages indexes).
apt-ftparchive \
    -o "APT::FTPArchive::Release::Origin=splitway" \
    -o "APT::FTPArchive::Release::Label=splitway" \
    -o "APT::FTPArchive::Release::Suite=$SUITE" \
    -o "APT::FTPArchive::Release::Codename=$SUITE" \
    -o "APT::FTPArchive::Release::Components=$COMP" \
    -o "APT::FTPArchive::Release::Architectures=$ARCHES" \
    release "dists/$SUITE" > "dists/$SUITE/Release"

if [ -n "$KEY" ]; then
    rm -f "dists/$SUITE/InRelease" "dists/$SUITE/Release.gpg"
    gpg_sign --clearsign -o "dists/$SUITE/InRelease" "dists/$SUITE/Release"
    gpg_sign -abs -o "dists/$SUITE/Release.gpg" "dists/$SUITE/Release"
    echo "apt repo signed with key $KEY -> $ROOT/dists/$SUITE/{InRelease,Release.gpg}"
else
    echo "apt repo built (unsigned) -> $ROOT/dists/$SUITE/Release"
fi
