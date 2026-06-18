# Roadmap

Reflects code state as of 2026-06-12.

Process: one phase = one branch = one PR into `dev` (workflow rules in `CLAUDE.md`); per-phase implementation prompts live in `docs/prompts/`.

Target features, wanted as early as possible: **NixOS support, macOS support, OpenVPN support, primitive GUI** (enable/disable + config selection). Hard constraint: no shortcuts at the expense of code quality.

Ordering principle: macOS + OpenVPN multiply *platform × VPN* implementations. Adding them on the current `DnsBackend` trait (which mixes VPN detection with rule application) would bake the mixed design into three implementations and force a triple rewrite later. So: split the abstractions **before** multiplying backends. Likewise, a GUI that shells out with sudo per click is a quality shortcut — the GUI waits for IPC and never holds privileges itself.

The macOS requirement also resolves the old open decision: a one-shot binary driven by systemd dispatcher units cannot port to macOS (no systemd). The daemon gets **built-in monitoring** with a per-platform event source; systemd unit and launchd plist become thin deployment artifacts.

## Phase 0 — Foundation + CI

Goal: make the core testable and failure-safe. Cheap, unblocks everything.

- Extract DNS parsing from `detect_vpn` into a pure function, cover with unit tests
- Rollback in `apply_rules`: if the domain step fails after DNS is set, revert to pre-apply state
- Resolve binaries via PATH or config instead of hardcoded `/usr/bin/resolvectl` (this is also the NixOS runtime blocker; `nmcli` is already PATH-based — inconsistent)
- Backend logging through `log` instead of `println!`
- Cleanup: unused `ConfigParseError::Unresolve`, `parse_command(self)` ignoring `self`, `vpn_ip` in README config example
- CI: fmt + clippy + test on `ubuntu-latest` and `macos-latest` runners (macOS compiles from day one, `todo!()` stubs stay honest)

**Done when:** `cargo test` green in CI on both runners; a failed apply leaves the system in its pre-apply state.

## Phase 0.5 — NixOS packaging

Small, independent, can land in parallel with Phase 0 (after the PATH fix).

- `flake.nix`: package + devShell
- NixOS module skeleton (service definition; fleshed out when the daemon becomes real in Phase 2)

**Done when:** `nix build` produces working binaries; `nix develop` gives a dev shell.

## Phase 1 — Abstraction split

Goal: separate VPN *detection* from DNS *rule application*, so every later platform/VPN addition is one small testable unit.

- `VpnDetector` trait: detect VPN + expose an event stream (interface up/down). First impl: NetworkManager (Linux), event source = NM D-Bus signals via `zbus`
- `DnsBackend` trait shrinks to apply/revert/status. First impl: systemd-resolved (`resolvectl`)
- `tokio` introduced here (event stream + later IPC share the runtime)

**Done when:** existing one-shot behavior works unchanged on the new traits; detector unit-testable without live `nmcli`.

## Phase 2 — Real daemon + IPC

Goal: the headline feature — auto-apply on VPN up, auto-revert on down — plus runtime control.

- Daemon event loop over `VpnDetector` stream; privileged operations live only here
- Unix socket, JSON-lines protocol
- `splitway-cli` (currently a stub): enable/disable, add/remove domain, status, reload config
- Deployment artifacts: systemd unit (Linux), launchd plist (macOS, activated in Phase 3)

**Done when:** connecting the VPN applies rules with no manual command; disconnecting reverts; CLI controls a running daemon.

## Phase 3 — Backend breadth (OpenVPN, macOS)

Each item is now a small isolated impl thanks to Phase 1.

- **3a. OpenVPN via NetworkManager** — cheapest: reuse the NM detector, handle `tun*` interfaces and OpenVPN-specific DNS fields
- **3b. macOS** — `VpnDetector` via `scutil` (utun interfaces, DNS from `scutil --dns`); `DnsBackend` via `/etc/resolver/<domain>` files + mDNSResponder cache flush. Live-tested on real hardware (available)
- **3c. OpenVPN standalone** — separate detector: OpenVPN management interface (preferred) or pushed-DNS parsing; more work, scheduled after 3a/3b prove the abstraction

**Done when:** GlobalProtect + OpenVPN(NM) work on Linux; at least GlobalProtect or OpenVPN works on macOS end-to-end.

## Phase 4 — Primitive GUI

Goal: enable/disable toggle, read-only status, config-file selection, and config editing. Still deliberately minimal — no tray icon, notifications, or per-domain status.

- Talks to the daemon over the same IPC socket as the CLI — zero duplicated logic, zero privileges in the GUI process. This rules out the GUI writing the config file itself (a second writer racing the daemon's own writes, and impossible against a root daemon)
- Editing the config over IPC needs a small, additive protocol extension: get/set-config verbs handled by the daemon's single-writer state actor, plus a `PROTOCOL_VERSION` bump. The toggle, status, and domain editing already fit the existing verbs
- Stack: `egui` (pure Rust, trivially cross-platform Linux/macOS, fastest to ship). Alternative if more native feel is wanted later: `iced`. GTK4/libadwaita dropped — poor macOS story
- Depends on Phase 2 (IPC) only; can start in parallel with Phase 3

**Done when:** toggle, status, config picker, and config editing work on Linux and macOS against a live daemon — all over IPC, with no privileges in the GUI.

## Phase 5 — GUI usability: live config, interface selection, polish

Goal: make the Phase 4 GUI correct and presentable — config changes take effect live, the VPN interface is picked from a list, and the window looks decent.

- Daemon: re-arm the VPN watch live when `vpn_name`/`vpn_backend`/`openvpn` change (via `SetConfig`/`ReloadConfig`), so the restart-on-`vpn_name` caveat disappears and `vpn_up` reflects the configured interface immediately. The existing detectors are reused unchanged — only their lifecycle (start/stop/restart) becomes dynamic, with the old interface reverted on switch and no half-configured state
- Daemon: a `ListInterfaces` IPC verb enumerating local interfaces (name + up/down, VPN-like flagged) so the GUI can offer an interface picker without itself touching the platform or holding privileges. Additive `PROTOCOL_VERSION` bump
- GUI: `vpn_name` becomes a picker over the live interface list (free-text fallback kept); a Resync button (re-read config + reconcile + refresh the view); immediate refresh after every change; and a real visual pass (opaque panel, grouped sections, aligned fields — the current build renders with a transparent background)
- Still a pure IPC client: zero privileges, zero duplicated logic. Builds directly on Phase 4

**Done when:** changing `vpn_name` in the GUI re-points auto-apply with no daemon restart and `vpn_up` tracks it; the interface picker lists present interfaces; Resync works; the GUI looks presentable on Linux and macOS against a live daemon.

## Phase 6 — Packaging & release

Goal: distributable packages + a release pipeline. Deliberately deferred until here: releasing before the daemon is real (Phase 2) would ship a one-shot CLI that misses the headline feature and burns first impressions.

- Tag-triggered release CI: `cargo build --release` for x86_64/aarch64, attach binaries to a GitHub Release
- Packages: deb / rpm / pacman / AppImage / nix (pattern reusable from `stslex/claude-desktop-linux`)
- apt/dnf repositories hosted on GitHub Pages for `apt install` / `dnf install`
- First public tag `v0.1.0` only after Phase 2 (working daemon)

A lightweight tag→build→artifacts workflow may land earlier as a small standalone PR (build only, no public announcement) if useful, but the full distribution story stays in this phase.

## Later (explicitly out of near-term scope)

- Windows support (stub remains)
- Proxy route targets (SOCKS5/VLESS) — requires a `RouteTarget` abstraction, not DNS-only
- Automatic discovery of related domains
- Richer GUI (rule editing, per-domain status)
