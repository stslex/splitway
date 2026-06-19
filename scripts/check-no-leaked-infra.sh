#!/usr/bin/env bash
#
# check-no-leaked-infra.sh — lightweight guard against committing real
# infrastructure data (a leak class that previously reached the repo: real
# resolver IPs and internal domain names pasted from `resolvectl status` /
# `nmcli` / `scutil --dns` verification output).
#
# It scans TRACKED files only (via `git grep`) for two high-signal, low-noise
# patterns. It deliberately does NOT try to flag every RFC 1918 address: the
# 10.x / 192.168.x ranges are both real-network ranges AND the conventional
# documentation stand-ins, so a blanket IP rule is noisy and unenforceable.
# Use redacted placeholders instead — see the "Redaction" section in CLAUDE.md.
#
# Patterns flagged:
#   1. Internal / custom-TLD domains (.corp/.lan/.intranet/.internal/.home) —
#      these are never legitimate documentation placeholders. Use example.com,
#      *.example, or corp.example.com instead.
#   2. Pasted resolver-dump signatures (a `resolvectl status` / `scutil --dns`
#      block carries these field labels verbatim, which never belong in tracked
#      files — fixtures use the per-field forms `IP4.DNS[n]:` / `nameserver[n]:`).
#
# Exit non-zero on any match. To allowlist a deliberate occurrence, extend the
# `:!path` exclusions below.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Files that legitimately describe these patterns (policy + this scrub's spec)
# rather than containing leaked data.
EXCLUDES=(
  ':!scripts/check-no-leaked-infra.sh'
  ':!CLAUDE.md'
  ':!docs/prompts/security-scrub.md'
  ':!.github/workflows/ci.yml'
)

status=0

# 1. Internal / custom-TLD domains.
if git grep -nIE '\b[a-z0-9-]+\.(corp|lan|intranet|internal|home)\b' -- "${EXCLUDES[@]}"; then
  echo "ERROR: internal/custom-TLD domain found in a tracked file." >&2
  echo "       Replace it with a placeholder (example.com, *.example, corp.example.com)." >&2
  status=1
fi

# 2. Pasted resolver-dump signatures.
if git grep -nIE '(Current DNS Server|DNS Servers|DNS Domain)[[:space:]]*:|^[[:space:]]*Link[[:space:]]+[0-9]+[[:space:]]*\(' -- "${EXCLUDES[@]}"; then
  echo "ERROR: a pasted resolvectl/scutil status dump was found in a tracked file." >&2
  echo "       Redact it to placeholder values before committing (see CLAUDE.md)." >&2
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "check-no-leaked-infra: OK (no internal domains or pasted resolver dumps found)."
fi

exit "$status"
