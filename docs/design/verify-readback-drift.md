# Live DNS read-back + drift detection (`Verify`)

The decision behind Phase 5d: a `Verify` verb that reads the **live** per-link DNS
state back from the system and reports **drift** from what the daemon believes it
installed. This is *reality* alongside `status`'s *belief*.

## The agreement

- **`status` stays cheap belief; `Verify` is the explicit reality check.** Status
  never reads the system back (the [architecture](../architecture.md) §1/§2
  invariant: status is mapped from the daemon's own reconcile decision). Read-back
  is on-demand only, behind a separate verb a client calls on refresh.
- **`DnsBackend::status(&self, &str) -> Result<(), _>` is removed, replaced by
  `read_link_state(&self, &str) -> Result<LinkDnsState, _>`.** The old `status`
  shelled `resolvectl status` only to discard the output; it had **no production
  callers** (the CLI/daemon `status` subcommands go through `Request::Status`). So
  this is a replacement, not the "sibling `read_link_state`" ROADMAP 5d sketched —
  there was nothing worth keeping alongside.
- **The drift comparison is a pure, total function** (`compare_drift`) over the
  wire types (`LinkDnsState` + `Option<AppliedInfo>` → `DriftVerdict`), unit-tested
  without any I/O — mirroring 5b's split of a pure parser + a thin backend method.
- **Normalized so cosmetic differences are never false drift.** Servers compare by
  canonical IP equality (case + IPv6 zero-compression) falling back to a case-fold;
  domains compare suffix-aware via `domain::domain_covers` (reusing 5b's matching),
  with the parser stripping a leading `~` so a routing-only `~example.com` and a
  bare `example.com` are one domain. Two `resolvectl`-format edge cases the parser
  handles so they are not mis-read as drift: a server token's `:port` / `%ifname` /
  `#SNI` decorations (`man resolvectl`'s `ADDRESS[:PORT][%ifname]#SNI`, IPv6
  bracketed) are stripped to the bare IP; and the route-all marker `~.` (parsed to
  the root `.`) is treated by `compare_drift` as covering **every** believed domain
  — a full-tunnel link that legitimately carries `~.` is in sync, not drift.

## Scope / out of scope

- **In:** parse `resolvectl status <iface>` (Linux); reconstruct from the managed
  `/etc/resolver` files (macOS, best-effort); a pure drift verdict; the `Verify`
  verb + `splitway verify`.
- **Out:** auto-remediation / reconcile-on-drift — this phase is **observability
  only**; a detected drift is reported, never acted on. Reachability / IP routing
  stays out (the [DNS-vs-routing boundary](../architecture.md) §3). Folding
  read-back into `status` is explicitly rejected (it would make the hot path shell
  out on every poll).

## Notable tradeoffs

- **A read-back failure degrades to an empty `live` + the verdict computed against
  it — never an IPC `Error`** (matching `CheckDomain`'s degrade-to-`None` ethos).
  Consequence: an empty `live` is ambiguous — read-back-unavailable vs a link that
  genuinely carries no Splitway DNS are indistinguishable on the wire. We
  **rejected a fourth `DriftVerdict::Unknown` variant**: the observable outcome
  ("the live state does not match belief") is the same either way, and the CLI
  surfaces the empty-live hint in words. `DriftVerdict` stays three variants
  (`NotApplicable` / `InSync` / `Drifted`).
- **`Verify` runs off the actor** via the detached `tokio::spawn` +
  `spawn_blocking` pattern `CheckDomain` uses, bounded by its own semaphore — so a
  slow or hung `resolvectl status` can never stall VPN reconciliation, and a burst
  of `verify` cannot starve `check` (separate limits).
- **macOS is best-effort and unverified on hardware:** there is no per-link DNS
  block, so the "live" state is reconstructed from the `/etc/resolver/<domain>`
  files Splitway wrote (keyed by domain, not interface — the interface argument is
  advisory). Windows is unsupported (inherits the trait default clean error).

## Links

- [`architecture.md`](../architecture.md) §1 (status is belief, read fresh), §2
  (clients render reported state; "applied" has two meanings), §3 (DNS-vs-routing
  boundary).
- Builds on the 5b live-read seam (`DnsBackend::resolve`, `linux/query.rs`) and
  reuses 5b's domain normalization (`splitway-shared/src/domain.rs`).
