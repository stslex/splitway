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
#      Exception: the `resolvectl status` parser legitimately matches these labels
#      in code and tests its parsing with synthetic-value fixtures in that format
#      (see DUMP_EXCLUDES); its IP *values* stay guarded by check 3 below, which
#      now also covers the `Current DNS Server:` / `DNS Servers:` lines.
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
  ':!.github/workflows/ci.yml'
)

# The `resolvectl status` parser (and its synthetic fixtures) must reference the
# `Current DNS Server:` / `DNS Servers:` / `DNS Domain:` field labels verbatim —
# matching them is its whole job — so it is exempt from the dump-signature check
# (2) only. It stays subject to the internal-domain check (1) and, crucially, to
# the placeholder-IP check (3), which now scans its server lines too, so a real
# resolver IP pasted into a fixture is still caught.
DUMP_EXCLUDES=(
  "${EXCLUDES[@]}"
  ':!splitway-daemon/src/backend/linux/status.rs'
)

status=0

# 1. Internal / custom-TLD domains.
if git grep -nIE '\b[a-z0-9-]+\.(corp|lan|intranet|internal|home)\b' -- "${EXCLUDES[@]}"; then
  echo "ERROR: internal/custom-TLD domain found in a tracked file." >&2
  echo "       Replace it with a placeholder (example.com, *.example, corp.example.com)." >&2
  status=1
fi

# 2. Pasted resolver-dump signatures. The `resolvectl status` parser is exempt
#    (DUMP_EXCLUDES) — it must match these labels — but its IP values stay guarded
#    by check 3, which now scans the `DNS Servers:` / `Current DNS Server:` lines.
if git grep -nIE '(Current DNS Server|DNS Servers|DNS Domain)[[:space:]]*:|^[[:space:]]*Link[[:space:]]+[0-9]+[[:space:]]*\(' -- "${DUMP_EXCLUDES[@]}"; then
  echo "ERROR: a pasted resolvectl/scutil status dump was found in a tracked file." >&2
  echo "       Redact it to placeholder values before committing (see CLAUDE.md)." >&2
  status=1
fi

# 3. DNS-server / interface / route fields must use placeholder IPs.
#    This covers the PRIMARY leak class this scrub addressed: pasted
#    `nmcli device show` (`IP4.DNS[n]`/`IP4.ADDRESS`/`IP4.GATEWAY`/`IP4.ROUTE`),
#    `scutil --dns` / resolv.conf (`nameserver`), and OpenVPN (`dhcp-option DNS`)
#    output whose IPv4 fields are real — which checks 1 and 2 miss when the dump
#    carries only real IPs and no internal-domain suffix. The allowlist is the
#    RFC 5737 documentation ranges plus the conventional stand-ins already used
#    across the fixtures; any other IPv4 on such a field line fails. (IPv6 doc
#    ranges 2001:db8::/fd00::/fe80:: are placeholders and are not checked.)
#    Limits (heuristic, acceptable for an optional guard): assumes one IPv4 per
#    field line, and cannot flag a real IP that happens to fall inside a stand-in
#    range (10.0.0./10.8.0./10.9.0./192.168.1.). The CLAUDE.md redaction policy
#    plus human review remain the primary defense.
#    The `resolvectl status` server labels (`Current DNS Server:` / `DNS Servers:`)
#    are included so the read-back parser's fixtures — exempt from check 2 — still
#    have their IP values enforced here. (`DNS Domain:` carries names, not IPs, so
#    it is not listed.)
DNS_FIELD='(IP4\.DNS\[|IP6\.DNS\[|IP4\.ADDRESS\[|IP4\.GATEWAY|IP4\.ROUTE\[|nameserver|dhcp-option DNS|Current DNS Server:|DNS Servers:)'
ALLOW_IP='(192\.0\.2\.|198\.51\.100\.|203\.0\.113\.|10\.0\.0\.|10\.8\.0\.|10\.9\.0\.|192\.168\.1\.|1\.2\.3\.4|1\.1\.1\.1|1\.0\.0\.1|2\.2\.2\.2|3\.3\.3\.3|8\.8\.8\.8|8\.8\.4\.4|9\.9\.9\.9|0\.0\.0\.0|127\.0\.0\.1|255\.255\.)'
dns_field_hits=$(git grep -nIE "$DNS_FIELD" -- "${EXCLUDES[@]}" \
  | grep -E '([0-9]{1,3}\.){3}[0-9]{1,3}' | grep -vE "$ALLOW_IP" || true)
if [ -n "$dns_field_hits" ]; then
  printf '%s\n' "$dns_field_hits" >&2
  echo "ERROR: a DNS/interface/route field uses a non-placeholder IPv4 — this looks" >&2
  echo "       like real captured nmcli/scutil/resolv output. Replace with RFC 5737" >&2
  echo "       placeholders (192.0.2.x / 198.51.100.x), or extend the allowlist if" >&2
  echo "       it is a deliberate stand-in (see CLAUDE.md)." >&2
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "check-no-leaked-infra: OK (no internal domains, pasted dumps, or real DNS-field IPs)."
fi

exit "$status"
