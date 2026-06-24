#!/usr/bin/env bash
# Build (and, given a key, GPG-sign) a dnf/yum repository for ONE channel.
#
#   build-dnf-repo.sh <repo-root> [gpg-key-id]
#
# <repo-root> is e.g. gh-pages/rpm/release, holding the channel's .rpm files
# (multi-arch in one repo — dnf filters by arch). The caller must have already
# placed/refreshed the .rpm files (this script never deletes them — "merge,
# never wipe"). It regenerates repodata/ with createrepo_c, and, when a
# gpg-key-id is given, also detach-signs the rpms in place (rpm --addsign) and
# writes repodata/repomd.xml.asc.
#
# Used by BOTH the PR signed-repo test (ephemeral key) and the real publish job.
set -euo pipefail

ROOT="${1:?usage: build-dnf-repo.sh <repo-root> [gpg-key-id]}"
KEY="${2:-}"

[ -d "$ROOT" ] || { echo "error: $ROOT does not exist" >&2; exit 1; }

if [ -n "$KEY" ]; then
    # Header-sign every rpm in place (idempotent; re-signing is harmless). %_gpg_name
    # selects the key; loopback pinentry feeds GPG_PASSPHRASE non-interactively.
    shopt -s nullglob
    rpms=("$ROOT"/*.rpm)
    if [ ${#rpms[@]} -gt 0 ]; then
        rpm \
            --define "_gpg_name $KEY" \
            --define "__gpg_sign_cmd %{__gpg} gpg --batch --no-armor --pinentry-mode loopback --no-secmem-warning -u %{_gpg_name} -sbo %{__signature_filename} %{__plaintext_filename}" \
            --addsign "${rpms[@]}"
    fi
fi

# (Re)generate repodata over all rpms in the channel.
createrepo_c --update "$ROOT"

if [ -n "$KEY" ]; then
    rm -f "$ROOT/repodata/repomd.xml.asc"
    gpg --batch --yes --pinentry-mode loopback --default-key "$KEY" \
        --detach-sign --armor "$ROOT/repodata/repomd.xml"
    echo "dnf repo signed with key $KEY -> $ROOT/repodata/{repomd.xml,repomd.xml.asc}"
else
    echo "dnf repo built (unsigned) -> $ROOT/repodata/repomd.xml"
fi
