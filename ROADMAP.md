# Roadmap

Reflects code state as of 2026-06-22.

Goal: finish the DNS-split solution to a shippable **v1 for Linux + macOS** —
complete the verification / business-logic work, package it, and replace the
interim GUI with a native one. Anything past that (other VPN backends, proxy
route targets) is deliberately deferred; see [Later](#later).

Process: one phase = one branch = one PR into `dev` (workflow rules in
`CLAUDE.md`). Implementation prompts are ephemeral and **not committed**; durable
design lives in this file (the plan), `docs/architecture.md` (cross-cutting
invariants), and `docs/design/` (per-feature decisions). Hard constraint: **no
shortcuts at the expense of code quality** — no phase trades correctness, tests,
or a clean abstraction for speed.

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
- **Phase 5 — live config, interface selection, verification (belief).** The
  usability scope **plus** exposing state the daemon already computes — all on one
  `PROTOCOL_VERSION` bump (to 3), egui kept minimal:
  - Daemon re-arms the VPN watch **live** when `vpn_name` / `vpn_backend` /
    `openvpn` change (the restart-on-`vpn_name` caveat is gone; the old interface
    is reverted on switch with no half-configured state).
  - A `ListInterfaces` verb enumerates local interfaces (name + up/down, VPN-like
    flagged) so the GUI offers a picker without touching the platform.
  - The **belief surface**: an applied snapshot in `StatusInfo` (interface +
    domains + DNS servers), a `RoutingState` enum mirroring the `desired()`
    branches (disabled / no domains / VPN down / no DNS from VPN / applied /
    apply-failed), and `detector_health` — what the daemon *intends*, not yet a
    read-back of reality.
  - GUI: `vpn_name` picker over the live interface list (free-text fallback kept),
    a Resync button, immediate refresh after every change, and an opaque grouped
    visual pass.

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

### Phase 5b — domain normalization + route-check (`CheckDomain`)

Read-back / drift detection (extending `DnsBackend::status()` to *return* the
live mapping) was originally bundled here but is **split into its own later
phase** — it is a separate, lower-priority change to the status model. It shares
the live-read backend seam this phase adds (`DnsBackend::resolve`), so it is
de-risked, not blocked, by the split.

- Domain normalization + case-insensitive dedup in `splitway-shared`, shared by
  the daemon and every client, so the daemon no longer trusts raw IPC input. The
  daemon normalizes on `add_domain` and `CheckDomain` (forward-only — existing
  config entries are not rewritten on load).
- A **`CheckDomain(host)`** verb answering two questions:
  - **Coverage** (pure, suffix-aware): resolvectl routes a domain *and its
    subdomains*, so `vault.example.com` is already covered by a configured
    `example.com`. Not-covered → offer to add it.
  - **Live resolution** via a new `DnsBackend::resolve`: Linux-strong (parsed
    `resolvectl query`, which attributes the answering link); macOS best-effort
    (no link/resolver attribution); unsupported-clean elsewhere. A resolution
    failure is never an error — the check still returns coverage.
  - Input is a pasted **URL** → parse host → normalize; CLI `splitway check <url>`.
  - **Boundary:** coverage + resolution are in scope; **reachability is not** —
    Splitway governs DNS, not IP routing (see `docs/architecture.md`).

### Phase 5c — config as the single source of truth

The config file becomes the authoritative state and the daemon stops caching it.
(Full invariant in `docs/architecture.md`.)

- The daemon **reads the config fresh on every operation** — **no in-memory
  config cache**; reconciliation is event-driven, not a hot loop. The only
  in-memory state is what the file cannot hold: the applied snapshot and the
  armed-watch parameters.
- **Atomic writes** (temp + rename) + **read-modify-write** on every mutation, so
  a concurrent external edit is never clobbered and a crash leaves no half-written
  file.
- A **file watcher** (inotify / FSEvents or the `notify` crate) picks up external
  hand-edits live — watch the *directory*, handle atomic-rename-replace, debounce
  self-writes.
- **Malformed = freeze**: keep the last-applied rules, surface the file as
  invalid, recover automatically when it parses again — never revert to a default.
- Config access behind a **testable abstraction** (no inline `fs::read` in
  `StateMachine`), so reconciliation is unit-testable without the filesystem.
- **NixOS:** the writable config lives in **`/var/lib/splitway/config.json` via
  `StateDirectory`**, not a module-generated read-only `/etc` file. The
  `nixosModule` provisions the state dir and passes `--config`; the model is
  **imperative** (the daemon owns the writable file, the GUI mutates at runtime),
  not declarative — options may *seed* an initial config but must not *lock* it.

### Phase 5d — verification (reality): live read-back + drift

Lower-priority; deferred out of 5b. Builds on the `DnsBackend::resolve` live-read
seam added in 5b.

- Extend `DnsBackend::status()` to **return** the live mapping (resolvectl on
  Linux, `/etc/resolver` on macOS) instead of only printing it — a sibling
  `read_link_state` reusing 5b's parsing approach — so the daemon can diff
  intended-vs-actual and surface drift: *reality* alongside Phase 5's *belief*.

### Phase 6 — packaging (distribution to other users)

The author's daily-driver path **already exists** via the Phase 0.5 flake +
`nixosModule` — that is the iteration channel. So this phase is **distribution to
other users, not the author's iteration unblock**: general-distro packaging gets
Splitway onto non-Nix machines, which is its own work because **generic Linux
binaries do not run on NixOS** (the dynamic linker is in the Nix store, not
`/lib64`) and immutable-`/usr` hosts need writable install paths.

- **One package** `splitway` containing daemon + cli + gui + the service unit, at
  a single version. This sidesteps the GUI↔daemon version matrix entirely: there
  are no separately-versioned packages to mismatch. `postinst` restarts
  `splitway.service` on upgrade so the running daemon always matches the new
  binaries; the existing version-peek (`VERSION_MISMATCH_PREFIX`) covers the brief
  upgrade window. **On NixOS the module is the single-version equivalent.**
- **Linux first:** tarball + apt / dnf / pacman repos on GitHub Pages, with **dev
  and release channels** as separate Pages subtrees. Pattern reusable from
  `stslex/claude-desktop-linux` — take the packaging/publishing half, drop the
  repackage half, and source artifacts from `cargo build --release`. Watch the
  Pages "full-site replace" trap that makes concurrent dev + stable deploys
  clobber each other.
- **Then macOS:** Homebrew tap / `.pkg` + launchd, with the Gatekeeper /
  notarization tail (unsigned vs Apple-Developer-signed) called out as a
  sub-decision.
- The dev channel is for iteration now; the public `v0.1.0` tag waits for Tauri.

### Phase 7 — native Tauri GUI

Build the Tauri frontend over the now-rich IPC and retire the egui GUI. A
**full window** is favored over a tray-popover: niri (and many Wayland
compositors) have **no system tray**, and the simultaneous-multi-VPN north-star
(see [Later](#later)) scales in a windowed / sidebar layout. The GUI must be
Wayland-native (egui and Tauri both are).

Decomposed into one-PR sub-phases so the truth contract is shared, not
reimplemented per frontend:

- **7a — `splitway-gui-core`** (done): extract the framework-agnostic GUI logic
  (the pure view-model **and** the truth-contract orchestration) into a crate
  depending on `splitway-shared` only, so both egui and the future Tauri backend
  drive one `GuiCore`. See [`docs/design/gui-core-extraction.md`](design/gui-core-extraction.md).
- **7b — Tauri shell + read-only view** (done): the `splitway-gui-tauri` backend
  hosts `GuiCore` and pushes the full view-model to a vanilla-TS frontend that
  renders it read-only (no mutations — those are 7c). gui-core gained `Verify`
  (live DNS read-back + same-cycle drift) and an owned, serializable
  `ViewModelSnapshot`; the protocol is unchanged. Built locally on niri (the
  webkit2gtk blank-window gotcha is resolved); the crate is kept out of the Nix
  default build until packaging in 7d. See
  [`docs/design/tauri-read-only.md`](docs/design/tauri-read-only.md). Its
  prerequisite — letting an unprivileged in-group user reach the root daemon
  without `sudo` (niri has no system tray, so the GUI runs as a normal user) —
  landed ahead of it as the opt-in socket group (`--socket-group` /
  `services.splitway.unprivilegedGui`); see
  [`docs/design/socket-group.md`](docs/design/socket-group.md).
- **7c — mutations through the contract** (done): the Tauri GUI mutates
  config/routing at parity with the daemon's existing write verbs
  (enable/disable, domain add/remove, config save, resync) plus the interactive
  `CheckDomain` one-shot — all **daemon-first, no optimistic UI**. A mutation
  command round-trips the daemon off the poll thread and fires a **refresh-now**
  wake; the poll thread stays the *sole* producer of view-models, so the change
  reaches the screen only via `view-model-changed` (the truth contract, enforced
  by construction). Pending/error/check are a distinct frontend request-lifecycle
  store. Frozen-on-malformed mutations are rejected with an on-disk-fix message
  and the frozen state is shown prominently. No protocol change (the verbs already
  exist at v6); egui stays a read/write reference, untouched. See
  [`docs/design/tauri-mutations.md`](design/tauri-mutations.md).
- **7d — visual design + window behavior** (done): the approved Variant B design as
  the real Tauri UI — full-window layout, the simplified interface-centric model
  (interface + domains; DNS auto-derived and shown read-only; no vpn-name/backend
  fields or settings screen, hidden in the GUI only — the daemon keeps every config
  field), every view-model variant designed (incl. the three full-window blockers),
  delete-undo + check-loading through the 7c truth contract, the in-app brand mark,
  and a stable niri `app_id` (`io.github.stslex.splitway` via `enableGTKAppId`). One
  authorized additive protocol bump (v6 → **v7**): `StatusInfo.detected_dns` exposes
  the selected interface's detected DNS independent of apply state, so the DNS
  readout is honest in the empty/disabled states too. See
  [`docs/design/tauri-design-window.md`](design/tauri-design-window.md). (Manual-DNS
  override — for VPNs that connect but push no DNS — is deferred as a real future
  daemon feature, not built here.)
- **7d-2 — bundling**: Nix packaging (two-stage frontend + Rust, `wrapGAppsHook3`,
  the blank-window workaround baked into the wrapper), distribution icons (tiled SVG
  + generated PNG sizes) + `.desktop` (`StartupWMClass` = the `app_id`), the flake
  `packages.<system>.splitway-gui`, bundling the IBM Plex OFL `woff2`, and the
  README GUI-install section. Split from 7d because its real proof — the *built*
  binary rendering for a fresh in-group niri user — is machine-bound.
- **7d-3 — macOS self-install**: the macOS counterpart of 7d-2's Linux bundling.
  An ad-hoc/unsigned `Splitway.app` (`.app` only — no signing, notarization,
  `.dmg`/`.pkg`, or `SMAppService`) that bundles the `splitway-daemon` + `splitway`
  helpers, a GUI LaunchDaemon plist (carrying `--socket-group splitway`), and a
  `bootstrap.sh`. Two health-keyed Tauri commands escalate via `osascript … with
  administrator privileges` (one native password prompt) to install/start
  (`NotRunning` → Install button) and disable (footer link) the root daemon — no
  terminal. The bundle path is additive (the Tauri bundler is invoked only by the
  build wrapper; `cargo build` / `nix build` never read `bundle`), and the commands
  keep the truth contract (do the work → refresh-now → never touch the VM). Split
  from 7d-2 because its real proof — the built `.app` driving the live install on
  macOS — is machine-bound. See
  [`docs/design/macos-self-install.md`](design/macos-self-install.md). (Homebrew —
  installing the same `.app` + binaries, with no competing `service` block — is the
  next phase.)

### Phase 8 — feature freeze + hardening

Fix issues surfaced while designing the earlier phases and correct decisions that
proved wrong. Explicit candidate: revisit the protocol's strict-equality
versioning now that packaging exists — the one-package model keeps daemon and
clients in lockstep for v1, so record whether to later relax to additive /
negotiated compatibility.

## Later

Explicitly deferred — out of the v1 scope above.

- **Multi-VPN** — three distinct directions, **explicitly unscheduled (no phase
  number)**:
  - **(A) Different VPN *types* / backends** — WireGuard, IKEv2, … i.e. more
    detectors on the existing trait.
  - **(B) VPN *profiles*** — several configured, one active at a time,
    switchable. Lighter: it leans on Phase 5's live re-arm.
  - **(C) Multiple *simultaneous* routings — the stated north-star.** Several VPNs
    active in parallel, each routing its own domains (resolvectl supports per-link
    routing domains). This pluralizes the whole data model: config → a list, watch
    → N self-contained units, applied → a per-interface map, reconcile / status →
    per-VPN, IPC → carries a `vpn_id`, GUI → per-VPN, plus a single→plural config
    migration. v1 is **migration-aware** — read-fresh + atomic writes (Phase 5c)
    de-risk that migration, and modeling the watch as a self-contained unit
    (Phase 5) is the stepping stone, so keeping v1 single paints no corner. Still
    **unscheduled**.
- **Proxy / `RouteTarget` route targets** (VLESS / Xray over SOCKS5). This is its
  own deliberate track, not a side-feature: split-DNS routes by *resolving* a
  domain through the VPN's DNS, whereas sending a domain through a SOCKS5 proxy
  needs a second data-path (transparent proxy / per-app SOCKS / a TUN into the
  upstream), not a new enum variant.
- **Automatic discovery** of related domains.
- **Windows.**
