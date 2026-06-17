# Phase 1 — Abstraction split

Implement Phase 1 from `ROADMAP.md`. Read `CLAUDE.md` for workflow rules first. The `platform-dns` skill (`.claude/skills/platform-dns/`) contains the NetworkManager D-Bus and resolvectl reference — use it, and verify exact D-Bus signatures via `busctl introspect` rather than trusting the reference blindly.

Scope: separate VPN *detection* from DNS *rule application*, add a VPN event stream. The event-driven daemon loop that consumes this stream is Phase 2 — do NOT build it here. All existing one-shot behavior (`run`/`revert`/`status`) must work exactly as before.

## Branch

Branch `phase-1-abstraction-split` from up-to-date `dev`.

## Tasks

### 1. Trait split in `splitway-shared/src/platform.rs`

```rust
#[derive(Debug, Clone)]
pub enum VpnEvent {
    Up(VpnInfo),
    Down { interface_name: String },
}

pub trait VpnDetector: Send + Sync {
    /// One-shot detection of the VPN on the given interface.
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError>;

    /// Subscribe to up/down events for the given interface.
    /// The detector owns the background task feeding the channel.
    fn watch(&self, interface: &str) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError>;
}

pub trait DnsBackend: Send + Sync {
    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError>;
    fn revert_rules(&self, interface: &str) -> Result<(), PlatformError>;
    fn status(&self, interface: &str) -> Result<(), PlatformError>;
}
```

- `detect_vpn` moves out of `DnsBackend` into `VpnDetector::detect`
- Both traits stay dyn-compatible (no async fn in traits; `watch` is a sync method returning a channel)
- `VpnInfo`, `PlatformError` stay as is; extend `PlatformError` only if a new variant is genuinely needed (e.g. `DbusError`)

### 2. Module layout in `splitway-daemon`

- `src/backend/` keeps `DnsBackend` impls only (resolvectl logic; remove `detect_vpn` from `backend/linux/backend.rs`, move `parser.rs` with it)
- New `src/detector/` mirrors the platform structure: `detector/linux/` — NetworkManager impl (`nmcli` detect + D-Bus watch), `detector/macos.rs` / `detector/windows.rs` — `todo!()` stubs that compile
- `create_backend()` → `create_dns_backend()` + `create_vpn_detector()` in the respective `mod.rs`

### 3. NetworkManager event stream (Linux)

- Dependency: `zbus` (latest stable), declared under `[target.'cfg(target_os = "linux")'.dependencies]` so macOS CI builds stay clean. `tokio` (features: `rt-multi-thread`, `macros`, `sync`) as a regular dependency
- `watch` spawns a background tokio task that: resolves the device by interface name (`GetDeviceByIpIface`), subscribes to its `StateChanged` signal, also handles `DeviceAdded`/`DeviceRemoved` on the manager so a tun device that appears *after* watch starts is picked up
- Map NM device states to events through a **pure function** (e.g. `fn transition(new_state: u32) -> Option<Transition>`): `ACTIVATED (100)` → emit `Up(detect(...))`, `DISCONNECTED (30)` / `DEACTIVATING (110)` / `FAILED (120)` / device removed → emit `Down`. Deduplicate repeated states (no two consecutive identical events)
- Unit-test the state-mapping and dedup logic without D-Bus. The D-Bus plumbing itself stays thin and untested (integration territory)

### 4. `watch` debug subcommand

Add `splitway-daemon watch`: subscribes via `VpnDetector::watch` and logs each event until Ctrl-C. This is the only runtime proof of the stream until Phase 2 and the manual-verification vehicle for the PR. Build the tokio runtime inside this command handler — do not convert the whole binary to async (that happens in Phase 2).

### 5. Cleanup (in-scope because main.rs is touched anyway)

- `main.rs:67` stray `println!` in `revert_dns_domain` → `log::error!`
- `show_status` panics on error → `log::error!` + `exit(1)`, consistent with the other commands

## Out of scope (do not implement)

- Daemon loop applying rules on events, IPC, config changes — Phase 2
- Switching `resolvectl domain` to routing-only `~domain` semantics — behavior change, deferred (noted in the platform-dns skill)

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macos)
- `run`/`revert`/`status` behavior unchanged
- `splitway-daemon watch` logs `Up`/`Down` on a real VPN toggle (manual check, steps + output documented in the PR description)
- State-mapping and dedup logic unit-tested; existing parser tests still pass
- macOS/windows compile with stubs for both traits

## Finish

Open a PR into `dev` titled `Phase 1: Abstraction split`, description: what moved where, trait rationale, manual verification log, done-criteria checklist.
