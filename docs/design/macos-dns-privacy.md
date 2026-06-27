# macOS DNS privacy — demote the hijacked default + scope the corp domains

The macOS backend used to do only half of split-DNS: it wrote
`/etc/resolver/<domain>` files so the configured corp domains resolve via the
corp DNS. That is the **scope** half. It is sufficient only when the VPN client
*scopes* its DNS to the tunnel. The observed corporate VPN does the opposite — a
**global DNS hijack**: it registers the corp resolver as the system **default**,
so every query that is *not* a configured corp domain would also go to the corp
resolver, over the tunnel. That is the privacy leak Splitway exists to prevent.

This phase adds the missing half — **demote** — so macOS reaches parity with the
Linux build: the corp resolver sees only the configured corp domains; everything
else leaves via the normal (physical) path and never traverses the tunnel.

> **Detection is structural and vendor-neutral.** Nothing in the code, tests, or
> this document names a VPN product or its services. Detection keys on the
> *shape* of the DNS configuration (a service whose resolver differs from the
> physical link's), never on a vendor string — so it generalises across clients.

## What the established facts were (proven on the author's Mac)

- **The corp resolver is the system default, not tunnel-scoped.** There is no
  resolver scoped to any `utun`; the active tunnel `utun` index even varies
  between sessions. So the previous interface-keyed detector (filter
  `scutil --dns` by a chosen `utun`) found nothing, for any interface choice.
- **Demote holds.** Overwriting the primary network service's `ServerAddresses`
  to a different resolver stuck (the client did not re-assert in a tight loop).
- **Demote is real, not cosmetic.** With the default demoted, no DNS for public
  names traversed the tunnel — the client does not transparently intercept `:53`.
- **`/etc/resolver/<domain>` is immune.** The client never touches those files; a
  scoped resolver there takes precedence over the default for its domain.
- **IP is already split by the client** — non-corp IPs route off-tunnel — so
  Splitway does **no** IP-route manipulation on macOS (unlike Linux's
  default-route demote). Splitway governs DNS only; see the boundary below.

(Synthetic stand-ins used throughout: corp DNS `192.0.2.53`, physical DHCP DNS
`198.51.100.1`, public-resolver override `203.0.113.9`, corp domains
`corp.example.com` / `jira.example.com`, interfaces `en0` / `utun0`.)

## The mechanism: demote + scope, transactional and reversible

On apply (VPN up, with corp domains configured), the macOS backend does both:

1. **Scope** — write `/etc/resolver/<domain>` → corp DNS for each corp domain
   (unchanged from before; on-tunnel, intended). Transactional: a mid-write
   failure restores every file to its prior bytes.
2. **Demote** — overwrite the primary network service's DNS
   (`State:/Network/Service/<primary>/DNS` `ServerAddresses`) with an off-tunnel
   **fallback** resolver, so non-corp DNS resolves off-tunnel. The prior value is
   **snapshotted to disk first** so it can be restored exactly.

Net effect: corp domains → corp DNS (on-tunnel); everything else → fallback DNS
(off-tunnel, invisible to the corp resolver).

The two steps are transactional **across both**: if the demote fails, the
resolver scope just written is rolled back, so the system is never left
half-changed (scoped but with the default still hijacked). The apply then
surfaces the error rather than recording success.

On revert (VPN down / disable / stop / shutdown) the backend removes every
managed `/etc/resolver` file **and** restores the demoted default from the
snapshot (then clears it). Restore runs on every exit path — the daemon already
reverts on `SIGTERM` (what `launchctl bootout` sends).

### The fallback resolver

The off-tunnel fallback defaults to the **physical primary interface's own DHCP
resolver** (the resolver that interface would use absent any override), which the
detector discovers. A config override — `fallback_dns` in the daemon config —
pins a specific public resolver instead (e.g. `["203.0.113.9"]`). The state
machine folds the override (if set) over the detector's value before handing the
effective fallback to the backend. The override is a root-config-file-only field
(the GUI does not edit it — out of scope this phase).

## Structural, vendor-neutral detection

Detection reads the SystemConfiguration dynamic store (via `scutil` in script
mode) and decides **structurally**:

- the **primary interface** — `State:/Network/Global/IPv4` `PrimaryInterface`
  (e.g. `en0`);
- every network service's DNS entry — `State:/Network/Service/.*/DNS`
  (`InterfaceName` + `ServerAddresses`).

The **physical service** is the one bound to the primary interface; its resolver
is the demote-target. A **VPN service** is any *other* service whose DNS differs
from the physical resolver — i.e. a non-physical resolver is in play. VPN is
**up** iff such a service exists; its resolver is the corp DNS. The decision is a
set comparison (order-insensitive), and it never references a `utun` name or a
vendor string.

### Why detection reads per-service DNS, not `State:/Network/Global/DNS`

This is load-bearing for stability. Splitway's *demote* overwrites the physical
primary service's DNS, which changes the **global default**. A detector keyed on
`State:/Network/Global/DNS` would therefore see the global default become the
fallback the moment our demote took effect, conclude "no VPN" (global == the
physical resolver), revert → the VPN's default returns → re-demote → **oscillation**.

Reading the VPN's corp DNS from its *own* service entry — which our demote does
**not** touch — keeps detection stable while the demote is in effect: the verdict
is unchanged before and after our own write. A unit test
(`detection_survives_our_own_demote_of_the_physical_service`) pins this.

## Decoupling the state machine from `vpn_name` (macOS only)

The state machine used to gate apply/revert on
`info.interface_name == config.vpn_name`. On macOS there is no stable, DNS-scoped
VPN interface to pin (the active `utun` varies), so that gate would never pass.
The gate is now branched on the backend's existing `reverts_globally()` seam:

- **global-revert backend (macOS)** → the interface gate is skipped; apply is
  driven by the DNS-model detection the detector already decided. The advisory
  `interface_name` rides along in `VpnInfo` but nothing keys on it.
- **per-interface backend (Linux)** → unchanged: the gate still requires
  `interface_name == vpn_name`.

The same branch covers the two read-only projections that used the gate
(`status().detected_dns` and `routing_state()`), so the macOS status readout is
honest. Linux behaviour is byte-for-byte unchanged (its `MockBackend` defaults
`reverts_globally` to false; all existing Linux state tests pass untouched).

## Reconcile on event

Reconnect / Wi-Fi toggle / sleep-wake / re-auth can re-install the corp default
or change the physical DHCP resolver. The SCDynamicStore watch fires on the
relevant DNS keys; the detector re-reads the model and re-emits `Up` whenever the
corp DNS **or** the demote-target changed (the watcher dedups only genuine
no-ops). The applied snapshot now also tracks the demote-target, so a change to
it forces a re-apply (re-demote to the new fallback) rather than being treated as
already converged. This is purely event-driven — no busy loop, no timer; one
re-apply per genuine change.

Because the observed client did **not** re-assert in a tight loop, no
sub-second re-apply guard is needed; a re-assert that arrives as a DNS-key change
is handled by the normal event path. The demote is idempotent (re-setting the
same fallback), and the snapshot is captured only on the *first* demote, so a
re-apply never overwrites the original prior state with our own fallback.

## Reversibility (the operational-safety contract)

- The pre-demote primary-service DNS is **snapshotted to disk** before the
  overwrite (`/var/run/splitway/dns-demote.snapshot`), so an unclean exit — the
  daemon `SIGKILL`ed between demote and a later revert — can still be undone on
  the next start. The snapshot uses `atomic_write` (intact on a crash mid-write).
- Restore rewrites exactly the snapshotted servers, or removes the key entirely
  when the service had no explicit prior DNS (so SystemConfiguration repopulates
  it from the real source), then clears the snapshot.
- Every system-network mutation is captured-before-write and rolled back on any
  failure path, mirroring the `/etc/resolver` apply's existing discipline — the
  machine is never left with a broken or half-demoted resolver.

## Testability

All `scutil` contact goes through two seams so the logic is unit-tested without
touching the live system:

- detection parses the dynamic-store dumps with **pure** functions over the
  synthetic-fixture dump shapes; the structural decision is a separate pure step;
- the demote/restore go through an injected `ScutilRunner` (the real impl shells
  out; tests inject a fake that **captures the exact script issued** and returns
  canned state) and a `SnapshotStore` (real = on-disk; tests = in-memory). The
  `apply_with` / `revert_with` wiring (including the rollback-on-demote-failure)
  is tested with both seams faked.

## Scope boundary — DNS only, not IP routing

Splitway governs **DNS**, not IP routing. The observed client's split-tunnel
already keeps non-corp **IP** traffic off the tunnel, so macOS does no route
manipulation. A full-tunnel / include-routes VPN that carried IP traffic through
the corp tunnel is **out of scope** (the same boundary as Linux): Splitway would
still split the DNS, but the IP path is the VPN client's concern. This is
deliberate and documented so the boundary is not mistaken for a gap.

## What is not built here

- **GUI changes** (interface-picker removal, a corp-domains / fallback-DNS UI) —
  a later phase. The macOS daemon no longer depends on `vpn_name` for
  correctness; the picker remains a benign no-op on macOS (the daemon
  auto-detects and ignores a picked `vpn_name`). The state machine is strictly
  daemon-side here.
- **Homebrew packaging** — a later phase; nothing ships until the live
  acceptance below passes.
- **The live verification itself** — see below. This phase is implementation +
  synthetic-fixture tests only.

## Deferred — live acceptance (run on the real VPN, not in this phase)

1. Packet-level, both interfaces, fresh random subdomains: public queries on the
   physical interface, **zero on the tunnel**; a corp host still resolves (via
   `/etc/resolver`).
2. Event robustness: reconnect / Wi-Fi toggle / sleep-wake → the demote re-holds.
3. Clean revert: stop / VPN-down restores the machine exactly as found.
4. `status` / `verify` / `check <corp-host>` / `check <public-host>` report the
   true state.

## Links

- [socket-group.md](socket-group.md) — the unprivileged-GUI access model.
- [macos-self-install.md](macos-self-install.md) — how the macOS daemon is
  installed/run (the privileged bootstrap this DNS work runs under).
- [linux-default-route-catch-all.md](linux-default-route-catch-all.md) — the
  Linux analogue (demote the link's DNS default-route so split-DNS holds).
- [architecture.md](../architecture.md) — the truth contract and the DNS-only
  boundary.
