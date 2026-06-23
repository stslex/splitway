# Linux split-DNS: disabling the default-route catch-all

The decision behind the `fix-linux-full-tunnel-dns-catch-all-leak` change: on a
**full-tunnel VPN**, applying per-link DNS servers + routing domains is *not
enough* to split DNS — the link is still systemd-resolved's catch-all, so every
name leaks to the VPN resolver. The fix makes the Linux backend disable that
catch-all, applies domains as routing-only, and teaches the read-back to detect
the leak.

## The problem

systemd-resolved routes a query that matches **no** link's routing domain to
every link flagged as the DNS *default route* (`Default Route: yes` /
`+DefaultRoute`). A full-tunnel VPN link carries the default *IP* route, so the
NM/VPN→resolved integration marks it as the DNS default route too. Splitway set
`resolvectl dns <iface> <servers>` + `resolvectl domain <iface> <domains>` but
never cleared that flag, so the link stayed the catch-all: a sibling like
`bitbucket.example.com` resolved through the VPN even though only
`jira.example.com` was configured. The configured routing domain was effectively
redundant — the split narrowed nothing.

Worse, the read-back was blind to it: `parse_resolvectl_status` collected only
`servers` + `routing_domains` and ignored the `Default Route:` line, so
`compare_drift` saw both matching belief and reported **InSync** while the split
was defeated.

## The agreement

- **`apply_rules` runs a third step: `resolvectl default-route <iface> false`**,
  after the dns + domain steps, so the link resolves only its routing domains and
  unmatched names fall to the system's normal resolver. It is funnelled through
  the **same rollback** the domain step already uses (a shared `run_apply_step`
  helper): a failure of any step after the dns step reverts the link, so a failed
  apply never half-configures it (the [architecture](../architecture.md) apply
  invariant). Run **last** because if it fails the link still has correct
  servers/domains (no worse than before the step) before the rollback fires.
- **`revert_rules` is unchanged.** `resolvectl revert <iface>` already clears the
  default-route override along with servers/domains, returning the link to its
  NM/DHCP-supplied value. The reset belongs to **apply only** — its lifetime is
  exactly the split's lifetime; when Splitway is not applying a split it must not
  leave a full-tunnel link de-defaulted.
- **Domains are applied routing-only (`~domain`).** A bare domain is *both* a
  search and a routing domain; `~domain` routes `*.domain` without polluting the
  search list (so a single-label `host foo` is not silently appended-and-routed).
  This is the behavior the [platform-dns reference](../../.claude/skills/platform-dns/SKILL.md)
  flagged as the correct-but-deferred semantic; it closes the residual
  search-suffix leak that `default-route false` alone does not. A `~`-prefixed
  config entry is not double-prefixed. `compare_drift` and the status parser
  already strip a leading `~` symmetrically, so this is not false drift.
- **The read-back learns the flag.** `LinkDnsState` gains
  `default_route: Option<bool>`, parsed from the `Default Route:` line and falling
  back to the `Protocols: +DefaultRoute` token when the explicit line is absent
  (older systemd-resolved output omits it). `compare_drift` reports a
  `Drifted { default_route_leak: true }` when the live link is a catch-all while
  the belief is a narrow split — so a link that re-becomes the catch-all (see
  tradeoffs) is detected, not silently InSync. A link counts as a catch-all by
  **either** signal: `default_route == Some(true)` **or** a live route-all routing
  domain (`~.`, parsed to `.`), since a VPN manager can install the catch-all
  either way and the suffix-aware coverage check would otherwise treat `.` as
  covering every believed domain and hide the leak. `None` with no route-all
  domain (read-back did not learn the flag, or macOS) is never a leak.

## Scope / out of scope

- **In:** Linux (systemd-resolved). The protocol bumps to **8** for the additive
  `LinkDnsState::default_route` + `DriftVerdict::Drifted::default_route_leak`
  fields (one package, one version — [architecture](../architecture.md) §4).
- **Out — macOS:** no analogous leak. `/etc/resolver/<domain>` files are
  per-domain; there is no link-level catch-all, so unmatched names already fall to
  the system resolver. The macOS read-back sets `default_route: None`, and
  `compare_drift`'s leak check is gated on `Some(true)`, so it never trips there.
- **Out — auto-remediation:** detection only, matching the `Verify` phase. A
  re-leak (below) is reported as drift, not auto-corrected.

## Notable tradeoffs

- **NetworkManager owns the flag and re-asserts `yes` on reconnect.** Splitway
  re-asserts `false` on each apply/reconcile and now *detects* a re-leak as drift,
  but does not continuously enforce it between applies. Durable enforcement (re-apply
  on an NM event, or a dispatcher hook) is follow-up.
- **`None` never fabricates drift.** An unknown default-route (older peer, a
  read-back failure, or macOS) is not a leak — mirroring the read-back's
  degrade-to-empty ethos. A belief that is *itself* route-all (a deliberate
  full-tunnel `~.`) is not a leak either.
- **Behavior change for full-tunnel users.** Names outside the configured domains
  now resolve via the system resolver (direct) instead of the VPN — which is the
  point of a split, but a visible change for anyone who relied on the catch-all.
  Assumes a working non-VPN system resolver exists.

## Links

- [`architecture.md`](../architecture.md) §3 (DNS-vs-IP-routing boundary — this
  fixes DNS resolver selection, not packet routing), §4 (one package/one version →
  the protocol bump), and the failed-apply-never-half-configures invariant.
- Extends [`verify-readback-drift.md`](verify-readback-drift.md): adds a third
  drift dimension (the catch-all leak) to `compare_drift` / `LinkDnsState`.
- [platform-dns reference](../../.claude/skills/platform-dns/SKILL.md): the
  `~domain` routing-only note, now acted on.
