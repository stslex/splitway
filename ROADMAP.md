# Roadmap

Reflects code state as of 2026-06-18.

Goal: finish the DNS-split solution to a shippable **v1 for Linux + macOS** —
complete the verification / business-logic work, package it, and replace the
interim GUI with a native one. Anything past that (other VPN backends, proxy
route targets) is deliberately deferred; see [Later](#later).

Process: one phase = one branch = one PR into `dev` (workflow rules in
`CLAUDE.md`); per-phase implementation prompts live in `docs/prompts/`. Hard
constraint: **no shortcuts at the expense of code quality** — no phase trades
correctness, tests, or a clean abstraction for speed.

## Ordering rationale

macOS + OpenVPN multiply *platform × VPN* implementations. Adding them on the
original `DnsBackend` trait (which mixed VPN detection with rule application)
would have baked the mixed design into three implementations and forced a triple
rewrite later — so the abstractions were split (Phase 1) **before** the backends
multiplied (Phase 3). Likewise, a GUI that shelled out with sudo per click would
be a quality shortcut, so the GUI is a pure IPC client that never holds
privileges. And because macOS has no systemd, a one-shot binary driven by
systemd dispatcher units could not port to it: the daemon got **built-in
monitoring** with a per-platform event source, leaving the systemd unit and
launchd plist as thin deployment artifacts.

## Done (shipped to `dev`)

The four target features — NixOS, macOS, OpenVPN, a primitive GUI — have all
landed, on the abstraction split that keeps each one a small isolated impl.

- **Phase 0 — Foundation + CI.** Pure DNS-parsing function with unit tests;
  rollback-on-failure in `apply_rules`; PATH/config binary resolution; `log`
  instead of `println!`; fmt + clippy + test CI on Linux and macOS runners.
- **Phase 0.5 — NixOS packaging.** `flake.nix` package + devShell, and the
  `nixosModules.default` systemd service.
- **Phase 1 — Abstraction split.** `VpnDetector` (detect + event stream) and
  `DnsBackend` (apply/revert/status) traits in `splitway-shared`; first impls
  NetworkManager (D-Bus via `zbus`) and systemd-resolved; `tokio` introduced.
- **Phase 2 — Real daemon + IPC.** Event-loop daemon (auto-apply on VPN up,
  auto-revert on down); Unix socket + JSON-lines protocol; `splitway-cli` as a
  real IPC client; systemd unit + launchd plist.
- **Phase 3 — Backend breadth.** 3a OpenVPN via NetworkManager; 3b macOS
  (`scutil` detector + `/etc/resolver` backend); 3c standalone OpenVPN over its
  management interface.
- **Phase 4 — Primitive GUI.** `splitway-gui` (egui): status, enable/disable,
  domain add/remove, and config editing — all over the CLI's IPC socket, zero
  privileges, behind the get/set-config `PROTOCOL_VERSION` bump.

## Frontend: egui is interim, Tauri is the target

The Phase 4 egui GUI is an **interim** frontend. The real one is **Tauri** (web
UI + Rust backend), built in Phase 7. Rationale: a native-feeling result, the
full web design ecosystem for the UI, and a clean fit with the existing model —
the GUI is just another zero-privilege IPC client over the control socket, so
the daemon needs no change to gain it. GTK4 stays dropped (poor macOS story) and
iced is no longer the planned path. The egui GUI gets only minimal upkeep until
Tauri replaces it.

## Upcoming

The sequence to v1, in order. Each is one phase = one branch = one PR.

### Phase 5 — live config, interface selection, verification (belief)

The original Phase 5 usability scope **plus** exposing state the daemon already
computes internally but does not surface. All of it rides **one**
`PROTOCOL_VERSION` bump (to 3); egui stays functional, not redesigned.

- Daemon: re-arm the VPN watch live when `vpn_name`/`vpn_backend`/`openvpn`
  change, so the restart-on-`vpn_name` caveat disappears and `vpn_up` reflects
  the configured interface immediately. The detectors are reused unchanged — only
  their lifecycle (start/stop/restart) becomes dynamic, with the old interface
  reverted on switch and no half-configured state.
- Daemon: a `ListInterfaces` verb enumerating local interfaces (name + up/down,
  VPN-like flagged) so the GUI can offer an interface picker without itself
  touching the platform or holding privileges.
- Daemon: surface what reconciliation already knows — an applied snapshot in
  `StatusInfo` (interface + domains + DNS servers, from the existing `Applied`
  struct), a `RoutingState` enum mirroring the existing `desired()` branches
  (disabled / no domains / VPN down / no DNS from VPN / applied / apply-failed),
  and `detector_health`. This is *belief*: what the daemon intends, not yet a
  read-back of what the system actually shows.
- GUI: `vpn_name` becomes a picker over the live interface list (free-text
  fallback kept); a Resync button (re-read config + reconcile + refresh);
  immediate refresh after every change; and an opaque, grouped visual pass (the
  current build renders with a transparent background).

### Phase 5b — verification (reality) + domain normalization

- Extend `DnsBackend::status()` to **return** the live mapping (resolvectl on
  Linux, `/etc/resolver` on macOS) instead of only printing it, so the daemon can
  diff intended-vs-actual and surface drift — *reality* alongside Phase 5's
  *belief*.
- Domain normalization + case-insensitive dedup in `splitway-shared`, shared by
  the daemon and every client, so the daemon no longer trusts raw IPC input.

### Phase 6 — packaging (pulled forward)

The gate that deferred packaging — no real daemon before Phase 2 — has expired,
so this comes ahead of the native GUI: a dev channel is needed to iterate on the
later phases.

- **One package** `splitway` containing daemon + cli + gui + the service unit, at
  a single version. This sidesteps the GUI↔daemon version matrix entirely: there
  are no separately-versioned packages to mismatch. `postinst` restarts
  `splitway.service` on upgrade so the running daemon always matches the new
  binaries; the existing version-peek (`VERSION_MISMATCH_PREFIX`) covers the brief
  upgrade window.
- **Linux first:** apt / dnf / pacman / nix repos on GitHub Pages, with **dev and
  release channels** as separate Pages subtrees. Pattern reusable from
  `stslex/claude-desktop-linux` — take the packaging/publishing half, drop the
  repackage half, and source artifacts from `cargo build --release`. Watch the
  Pages "full-site replace" trap that makes concurrent dev + stable deploys
  clobber each other.
- **Then macOS:** Homebrew tap / `.pkg` + launchd, with the Gatekeeper /
  notarization tail (unsigned vs Apple-Developer-signed) called out as a
  sub-decision.
- The dev channel is for iteration now; the public `v0.1.0` tag waits for Tauri.

### Phase 7 — native Tauri GUI

Build the Tauri frontend over the now-rich IPC and retire the egui GUI.

### Phase 8 — feature freeze + hardening

Fix issues surfaced while designing the earlier phases and correct decisions that
proved wrong. Explicit candidate: revisit the protocol's strict-equality
versioning now that packaging exists — the one-package model keeps daemon and
clients in lockstep for v1, so record whether to later relax to additive /
negotiated compatibility.

## Later

Explicitly deferred — out of the v1 scope above.

- **More VPN backends** beyond the current set (WireGuard, …).
- **Proxy / `RouteTarget` route targets** (VLESS / Xray over SOCKS5). This is its
  own deliberate track, not a side-feature: split-DNS routes by *resolving* a
  domain through the VPN's DNS, whereas sending a domain through a SOCKS5 proxy
  needs a second data-path (transparent proxy / per-app SOCKS / a TUN into the
  upstream), not a new enum variant.
- **Automatic discovery** of related domains.
- **Windows.**
