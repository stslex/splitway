# Docs sync — README / CLAUDE.md / ROADMAP to shipped state and the agreed plan

Read `CLAUDE.md` first. This is a **docs-only, standalone PR** (not a numbered
phase): no code changes. Two jobs:

- **Part A** — correct the few doc claims that no longer match the *shipped*
  code (a capability described as a stub/planned/"until Phase 2" that has in fact
  landed).
- **Part B** — re-plan `ROADMAP.md` to the agreed direction (the current
  ROADMAP still describes the old Phase 5/6 structure and egui-as-final).

Add this prompt itself to the repo as `docs/prompts/docs-sync.md` in the same PR
(matching how each phase's prompt lands with its phase).

## Branch

Branch `docs-sync` from up-to-date `dev`. PR into `dev`. English only.

## Part A — correct shipped-state drift

Each item is a known-stale claim found by `grep`. Verify against the code before
editing; do not introduce new claims beyond what has actually shipped.

1. **`README.md` (Nix section, the "stub until Phase 2" sentence).** False:
   `nix/module.nix` already defines a *real* `systemd.services.splitway`
   (`ExecStart = … run`, `RuntimeDirectory`, `SIGTERM`/`TimeoutStopSec`,
   `Restart`). Rewrite the sentence to describe the real service that
   `nixosModules.default` installs — drop "commented-out stub" and the
   "until Phase 2" framing entirely.
2. **`README.md` (Roadmap section summary + "Near-term priorities").** The named
   near-term items (NixOS packaging, macOS, OpenVPN, minimal GUI) have all
   shipped. Replace the one-line summary and the near-term list with the agreed
   near-term plan from Part B (finish the verification/business-logic work →
   Linux + macOS packaging → native Tauri GUI → hardening), and let the detail
   live in `ROADMAP.md` rather than duplicating it here.
3. **`CLAUDE.md` (crate list, `splitway-cli` line).** Drop "(stub until Phase 2)"
   — the CLI is a real IPC client over the daemon socket.
4. **`CLAUDE.md` (crate list).** It omits `splitway-gui`. Add it: the interim
   egui GUI, a pure IPC client with no privileges (native Tauri GUI planned — see
   Part B).
5. **`CLAUDE.md` (crate list, `splitway-shared` line).** It lists only
   `DnsBackend`. Both platform traits now live in
   `splitway-shared/src/platform.rs` — list `DnsBackend` *and* `VpnDetector`.

Leave claims that are still true untouched, in particular: README's "primitive
GUI" wording (the egui GUI genuinely is interim), and README's "runtime
switching is a planned follow-up" (still true; runtime config-file switching is
deferred — Part B).

## Part B — re-plan `ROADMAP.md`

Keep the existing framing that still holds: the **Process** block (one phase =
one branch = one PR into `dev`; per-phase prompts in `docs/prompts/`), the
**hard constraint** (no shortcuts at the expense of code quality), and the
ordering rationale. Update the "reflects code state as of …" date. Then restate
the roadmap so the following decisions are captured.

### Done (mark as shipped, don't re-describe in detail)

Phases 0 (foundation+CI), 0.5 (NixOS packaging), 1 (abstraction split:
`VpnDetector` / `DnsBackend`), 2 (real daemon + IPC), 3a/3b/3c (OpenVPN-via-NM,
macOS, standalone OpenVPN), 4 (primitive egui GUI) are all merged. Reframe the
document so "done" vs "upcoming" is unambiguous.

### GUI framework decision (record it)

egui (Phase 4) is the **interim** frontend. The real frontend is **Tauri**
(web UI + Rust backend). Rationale to capture: native-feeling result, the full
web design ecosystem, and a clean fit with the existing model — the GUI is just
another zero-privilege IPC client over the control socket, so the daemon needs
no change to gain it. GTK4 stays dropped (poor macOS story); iced is no longer
the planned path. The egui GUI gets only minimal upkeep until Tauri replaces it.

### Upcoming sequence (the "now" goal: finish the current DNS-split solution to a shippable v1 for Linux + macOS)

1. **(this PR) docs-sync.**
2. **Phase 5 — live config + interface selection + verification (belief).**
   The existing Phase 5 scope (live re-arm of the VPN watch, `ListInterfaces`,
   interface picker, Resync) **plus** surfacing what the daemon already computes
   but does not expose: an applied-snapshot in `StatusInfo`
   (interface + domains + DNS servers, from the existing `Applied` struct), a
   `RoutingState` enum (the existing `desired()` branches: disabled / no domains
   / VPN down / no DNS from VPN / applied / apply-failed), and `detector_health`.
   All wire changes ride **one** `PROTOCOL_VERSION` bump to 3. egui stays
   functional, not redesigned.
3. **Phase 5b — verification (reality) + domain normalization.** Extend
   `DnsBackend::status()` to *return* the live mapping (resolvectl on Linux,
   `/etc/resolver` on macOS) so the daemon can diff intended-vs-actual and
   surface drift. Plus domain normalization + case-insensitive dedup in
   `splitway-shared` (shared by the daemon and every client, so the daemon no
   longer trusts raw IPC input).
4. **Phase 6 — packaging (pulled forward).** The gate that deferred this
   (no real daemon before Phase 2) has expired. Ship **one package** `splitway`
   containing daemon + cli + gui + the service unit, at a single version — this
   sidesteps the GUI↔daemon version matrix (there are no separately-versioned
   packages to mismatch). `postinst` restarts `splitway.service` on upgrade so
   the running daemon always matches the new binaries; the existing version-peek
   (`VERSION_MISMATCH_PREFIX`) covers the brief upgrade window. **Linux first:**
   apt/dnf/pacman/nix repos on GitHub Pages with **dev + release channels** as
   separate Pages subtrees (pattern reusable from `stslex/claude-desktop-linux`
   — take the packaging/publishing half, drop the repackage half, source
   artifacts from `cargo build --release`; note the Pages "full-site replace"
   trap that makes concurrent dev+stable deploys clobber each other). **Then
   macOS:** Homebrew tap / `.pkg` + launchd, with the Gatekeeper/notarization
   tail (unsigned vs Apple-Developer-signed) called out as a sub-decision.
   The dev channel is for iteration now; the public `v0.1.0` tag waits for Tauri.
5. **Phase 7 — native Tauri GUI** over the now-rich IPC; the egui GUI is retired.
6. **Phase 8 — feature freeze + hardening.** Fix issues surfaced while designing
   the earlier phases and correct current decisions that proved wrong. Explicit
   candidate: revisit the protocol's strict-equality versioning now that
   packaging exists (the one-package model keeps lockstep for v1; record whether
   to relax to additive/negotiated compatibility later).

### Later (explicitly deferred — out of the v1 scope above)

- Multiple VPN backends beyond the current set (WireGuard, …).
- Proxy / `RouteTarget` route targets (VLESS/Xray over SOCKS5). Note *why* this
  is its own deliberate track and not a side-feature: split-DNS routes by
  *resolving* a domain through the VPN's DNS; sending a domain through a SOCKS5
  proxy needs a second data-path (transparent proxy / per-app SOCKS / a TUN into
  the upstream), not a new enum variant.
- Automatic discovery of related domains.
- Windows.

## Out of scope

- Any code change (this is docs-only). No new features, no protocol change.
- Rewriting docs that are already accurate, or describing unbuilt future in
  README's "Current state" (future belongs in `ROADMAP.md`).

## Done criteria

- A repo-wide reconcile of stale phrases: `grep -rniE "until phase|phase 2|stub|
  commented-out|planned|in progress|near-term" README.md CLAUDE.md nix/` turns up
  nothing that contradicts shipped code (claims still true may remain).
- `README.md` and `CLAUDE.md` crate descriptions match the actual four-crate
  workspace, including `splitway-gui` and both platform traits.
- `ROADMAP.md` reflects: phases 0–4 done; the Phase 5 / 5b / 6 / 7 / 8 sequence
  above; the egui-interim → Tauri-final decision with its rationale; the
  one-package + dev/release-channel packaging model; and the deferred multi-VPN +
  VLESS/Xray track.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` still pass (unchanged — docs only, but run them).
- `docs/prompts/docs-sync.md` (this file) is included in the PR.

## Finish

PR into `dev` titled `docs: sync README/CLAUDE/ROADMAP to shipped state and the
agreed plan`. Description: the Part A corrections (what was stale and the code
that disproves it), and the Part B re-plan (the captured decisions —
egui→Tauri, packaging-forward + one-package, the Phase 5/5b/6/7/8 sequence, the
deferred track). No screenshots, no manual verification log (docs only).
