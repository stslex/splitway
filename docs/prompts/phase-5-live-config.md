# Phase 5 — live config + interface selection + verification (belief)

Read `CLAUDE.md` first. **This file supersedes the previous
`phase-5-gui-usability.md`** (written before two decisions: egui is now the
*interim* GUI with a native Tauri GUI coming in Phase 7, so the heavy visual pass
is dropped; and the daemon's internal truth is now surfaced over IPC for
verification). Implement this scope.

Phase 4 shipped a working-but-rough egui GUI. This phase does three things, all
riding **one** `PROTOCOL_VERSION` bump to `3`:

1. **Config changes take effect live** — editing `vpn_name` / `vpn_backend` /
   `openvpn.*` re-points auto-apply with no daemon restart (the watch is armed
   once at startup today). This is the central, design-heavy task.
2. **Surface what the daemon already computes** — the applied DNS mapping, a
   self-explaining routing state, and detector health, so a client can *verify*
   what is happening, not just see a single `applied` bool.
3. **Pick the interface from a list** (`ListInterfaces`) and a **Resync** button.

The GUI stays a **pure IPC client — zero privileges, zero duplicated logic,
writes no files**. Its job this phase is to *exercise and display* the new
capabilities (it is the test harness for them before Tauri), **not** to look
good — see "GUI changes (minimal)".

## Branch

Branch `phase-5-live-config` from up-to-date `dev`. PR into `dev`. English only.
This is a large phase; landing it as iterative commits on one branch (as Phase 2
/ 3b / 4 did) is fine.

## The core problem (read before designing)

Read `splitway-daemon/src/daemon/mod.rs` (`run_async`) and
`splitway-daemon/src/daemon/state.rs`. Today the watch is wired once:
`create_vpn_detector(&config).watch(&interface)` is called in `run_async` at
startup; its `Receiver<VpnEvent>` is forwarded by a spawned task into the state
actor's `state_tx` as `StateCommand::Vpn`. The `StateMachine` is **passive**
(`run_state` owns the loop; the machine only processes commands), and its config
can change afterwards (`SetConfig`/`ReloadConfig`) **without the watch ever being
rebuilt**. `state.rs::warn_on_restart_only_changes` exists precisely because of
this — it logs "takes effect after a daemon restart" instead of acting. So
`StatusInfo.vpn_up` is driven only by events on the *startup* interface; after
editing `vpn_name`, `desired()` sees `interface_name != config.vpn_name`, reverts
the old interface, and can never apply to the new one. That is the root of the
"vpn_up looks wrong / isn't tied to the GUI's vpn_name" report.

Making config edits live therefore means giving the detector watch a **dynamic
lifecycle** — start, stop, restart — driven by config changes, instead of a
one-shot at boot.

## Design A — live re-arm of the VPN watch (justify in the PR)

The detectors are **reused unchanged** (NM, standalone OpenVPN, macOS); only
*when* a watch starts/stops changes. Move the watch lifecycle into the actor.

- **Give the `StateMachine` ownership of the watch.** Add to `new(...)` a clone
  of the actor's own `mpsc::Sender<StateCommand>` (so re-armed forwarding tasks
  can feed `StateCommand::Vpn` back in) and a field holding the current watch's
  cancel handle (a `tokio::task::AbortHandle`, or a cancel `oneshot`/token — see
  teardown below). Construct the `(state_tx, state_rx)` channel in `run_async`
  *before* `StateMachine::new`, pass `state_tx.clone()` in, and **move all arming
  into one `arm_watch()` method on the machine**: call it once at startup
  (cleanest inside `run_state` before the loop) and again on a config change.
  This deletes the separate forwarding-task spawn from `run_async` entirely.
- **`arm_watch()` does, in order:** tear down the current watch (cancel the old
  forwarding task → drops its `Receiver<VpnEvent>`), then `create_vpn_detector(
  &self.config).watch(&self.config.vpn_name)`; on `Ok`, spawn a forwarding task
  (`events.recv()` → `state_tx.clone().send(StateCommand::Vpn(_))`) and store its
  cancel handle, mark detector health Active; on `Err`, log, leave no watch
  running (auto-apply off), mark detector health `Error(_)`. An empty `vpn_name`
  tears the watch down and leaves none (the startup-empty behaviour).
- **Re-arm trigger.** In `commit()` and `reload_config()`, *after* the new config
  is adopted, if `vpn_name` / `vpn_backend` / `openvpn` changed vs the old config:
  reset `vpn_up = false` and `last_info = None`, `reconcile()` (which now reverts
  the old interface, because `desired()` no longer matches), **then** `arm_watch()`
  for the new config. Replace `warn_on_restart_only_changes` with this path (the
  warning becomes action). Revert-before-arm preserves the "no half-configured
  state" guarantee.
- **Sample immediately on re-arm** so `vpn_up` reflects the new interface's
  *current* state without waiting for the next up/down edge. **First check what
  the detectors already do**: if `watch()` already emits the current state as its
  first event, a separate sample double-applies — rely on that instead. If it
  does not, call the detector's one-shot `detect()` once after arming and feed
  the result through the normal `on_event` path, deduped against the first
  streamed event (the detectors already use a "sample after arming" / `Deduper`
  pattern — mirror it). Pick whichever avoids a double apply and say which in the
  PR.
- **Cancellation must actually release detector resources** (the NM D-Bus
  connection, the OpenVPN management socket). Cancelling the forwarding task drops
  the `Receiver`, which closes the channel; confirm each detector observes that
  promptly (it must `select!` on `tx.closed()` rather than only blocking on its
  own I/O — otherwise a standalone-OpenVPN management socket can linger until the
  next push). If a detector needs explicit teardown, give the forwarding task a
  cancel branch instead of a bare `abort()`. Document what you relied on per
  detector.
- **Rejected:** keeping the watch in `run_async` and poking it from the actor via
  a side channel — it splits the lifecycle across two owners and races config
  adoption against re-arm. Keep config change and re-arm in one place (the actor).

This removes the restart caveat, so the GUI's restart warnings and
`model::interface_change_needs_restart` **must go** (they would now be false).

## Design B — `ListInterfaces` over IPC

Add a read-only verb so the GUI can offer an interface picker without touching the
platform or holding privileges.

- `Request::ListInterfaces -> Response::Interfaces(Vec<InterfaceInfo>)`, with
  `InterfaceInfo { name: String, up: bool, vpn_like: bool }` — a small dedicated
  wire type in `splitway-shared/src/ipc.rs` (like `ConfigView`). `vpn_like` is a
  name-prefix heuristic (`tun` / `utun` / `wg` / `tap` / `ppp` / `gpd*`),
  advisory only — never a filter.
- **Enumeration** is unprivileged, read-only, and lives in a thin per-platform
  daemon module (Linux: `/sys/class/net/<n>/operstate` + flags, or `getifaddrs`;
  macOS: `getifaddrs`). Keep the platform I/O thin and untested; the
  **classification + sort + dedup** (`vpn_like`; order up-first then
  vpn-like-first; de-duplicate the multiple `getifaddrs` entries per interface) is
  **pure and unit-tested with fixtures**. Loopback is included but
  `vpn_like = false`; let the GUI sort/present, do not filter in the daemon.
- Handled in `StateMachine::on_request` like the other verbs. An enumeration
  error returns `Response::Error`, never a panic.
- A CLI `splitway interfaces` command is **optional**; may be skipped to keep the
  CLI minimal — note the choice.

## Design C — verification (belief): surface the daemon's internal truth

The daemon already computes more than `StatusInfo` exposes. Surface it (no new
machinery — map existing fields). All of this is part of the **same `v3` bump**.

Change `StatusInfo` (in `splitway-shared/src/ipc.rs`):

- **`applied: Option<AppliedInfo>`** replacing `applied: bool`, where
  `AppliedInfo { interface: String, domains: Vec<String>, dns_servers: Vec<String> }`.
  Source: the existing private `Applied` struct in `state.rs` (map it to the wire
  type in `status()`). `None` = not applied; `is_some()` recovers the old bool
  meaning. This answers "which domains route through which DNS right now".
- **`routing_state: RoutingState`** — a new enum computed in `status()` from the
  existing `desired()` branches + `needs_resync`:
  `Disabled` (`!config.enabled`), `NoDomains` (`vpn_hosts` empty), `VpnDown`
  (`!vpn_up`), `NoDnsFromVpn` (up but `last_info.dns_servers` empty), `Applied`
  (rules applied), `ApplyFailed` (`needs_resync` / last apply errored). Decide on
  an `InterfaceMismatch` variant: after live re-arm `config.vpn_name` always
  matches the watched interface when up, so it is effectively unreachable — keep
  it only as defence for the brief switch window, or drop it; say which.
- **`detector_health`** — e.g. `enum DetectorHealth { Active, Inactive, Error(String) }`,
  set by `arm_watch()`'s outcome and reported in `status()`. Lets a client say
  "the watch is up / down / failed to start" rather than guessing.
- Consider renaming `interface` → `configured_interface` for clarity now that
  `applied.interface` also exists (optional; we are already breaking the wire with
  `v3`). If kept, document that it is the *configured* name.

New wire types (`AppliedInfo`, `RoutingState`, `DetectorHealth`, `InterfaceInfo`)
get round-trip + back-compat tests, as `ConfigView` has.

## Protocol

- **Bump `PROTOCOL_VERSION` to 3.** Additive variants/types only; the existing
  version-peek + strict-equality handling (`daemon::ipc::process_line`,
  `VERSION_MISMATCH_PREFIX`) already covers skew — daemon/CLI/GUI upgrade in
  lockstep. (One package will keep them in lockstep; revisiting strict-equality is
  Phase 8, not now.)

## GUI changes (minimal — NOT a redesign)

egui is interim; the real UI is Tauri (Phase 7). Do the **minimum** to make the
new capabilities usable and to stop the window looking broken — no grouping
polish, no theming pass beyond the one fix below.

- **One visual fix only: render inside an opaque `egui::CentralPanel`** instead of
  the bare background-less root `ui()`, so the transparent-background look is gone
  (a packaged dev build in Phase 6 must not look broken). Nothing else visual.
- **Interface picker.** `vpn_name` becomes a combo populated from
  `ListInterfaces` (up first, `vpn_like` flagged), refreshed on connect, on the
  poll, and after Resync. Keep a free-text fallback and always preserve the
  currently-configured value even when absent/down (it may be a VPN not up right
  now). Selecting an interface is a normal edit saved via `SetConfig`.
- **Show the new status plainly** (this validates the verbs from a client):
  `routing_state`, the `applied` mapping (interface / domains / DNS servers), and
  `detector_health`. Crude text/labels are fine.
- **Resync button** (header): send `ReloadConfig`, then refetch
  `Status` + `GetConfig` + `ListInterfaces`. Define the unsaved-edit behaviour
  (discard after confirm, or keep per the existing guard) and say which.
- **Remove the now-false restart caveats**: the two `colored_label` notes in the
  config editor and `model::interface_change_needs_restart`.
- **Auto-refresh on every successful mutation** (enable/disable, add/remove, save,
  resync): immediately refetch `Status` + `GetConfig` (+ `ListInterfaces` where
  relevant); keep the periodic poll as backstop.
- New decision logic still goes in `model.rs` with tests (picker selection,
  resync action, routing-state/applied display); rendering stays untested
  plumbing.

## Failure modes (handle; unit-test the pure parts)

- **Re-arm to a missing / never-up interface:** watch starts (or sample is down);
  `vpn_up = false`; no apply; no error spam.
- **Re-arm where the new detector cannot start** (NM absent, bad
  `openvpn.management`): auto-apply off, `detector_health = Error`, IPC stays up,
  clear error to the client + log; the old interface was already reverted, daemon
  does not crash or strand rules. (`SetConfig` already rejects
  openvpn-without-management at the boundary; this covers runtime failures.)
- **Rapid successive config changes:** each re-arm cancels the previous forwarding
  task before spawning the next; no leaked detector tasks/sockets; final state
  matches the final config.
- **`ListInterfaces` enumeration error:** `Response::Error`, not a panic; the GUI
  shows the picker unavailable and falls back to free-text.
- **Resync while the daemon is down / version-skewed:** reuse the existing
  `ClientError` / version-mismatch banners.

## Out of scope (explicitly deferred)

- **Verification (reality) / drift detection** — extending `DnsBackend::status()`
  to return the live `resolvectl` / `/etc/resolver` mapping and diff
  intended-vs-actual — is **Phase 5b**. This phase exposes the daemon's *belief*
  only.
- **Domain normalization / dedup in `splitway-shared`** — **Phase 5b**.
- **Any GUI redesign** (grouping, hero toggle, human status line, theming) — that
  is the **Phase 7 Tauri** work; do not invest in egui visuals here beyond the
  opaque panel.
- Changing detection *logic* in any detector; any privileged operation in the GUI;
  runtime config-file switching; tray/notifications/autostart; multiple VPN
  backends; proxy/route targets.

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macOS).
- **Live switch (Linux), no restart:** changing `vpn_name` between two interfaces
  in the GUI re-points auto-apply, `vpn_up` tracks the configured interface, and
  the old interface is reverted on switch (no half-configured state). Manual log +
  before/after `resolvectl status` in the PR.
- The interface picker lists current interfaces (up flagged) and selecting one
  saves via `SetConfig`; a down/absent configured interface is still shown.
- `StatusInfo` exposes the applied mapping, `routing_state`, and
  `detector_health`; the GUI shows them; `applied` is `Some` exactly when rules
  are applied.
- Resync re-reads + reconciles + refreshes; every mutation refreshes immediately.
- `PROTOCOL_VERSION` is `3`, additive; new wire types round-trip; skew still
  surfaces as "update splitway".
- Pure logic unit-tested: the re-arm decision ("does this config delta require a
  re-arm?"), interface classification/sort/dedup, the `routing_state` mapping, and
  the GUI model additions. Daemon re-arm covered with a mock detector (old watch
  stopped, new armed, `vpn_up` reset/sampled, old interface reverted, arm-failure
  keeps IPC up + sets `detector_health = Error`).
- `README.md` updated: the GUI applies config changes live (drop the restart
  caveat), offers an interface picker, has a Resync button, and the daemon now
  reports the applied mapping / routing state / detector health.
- This file (`docs/prompts/phase-5-live-config.md`) replaces the old
  `phase-5-gui-usability.md` (delete the old file).

## Finish

PR into `dev` titled
`Phase 5: live config + interface selection + verification (belief)`.
Description: the re-arm lifecycle design (actor-owned watch + the
cancellation/teardown you relied on per detector + the sample approach you chose),
the `ListInterfaces` design, the `StatusInfo` additions, the Resync + unsaved-edit
decision, the `PROTOCOL_VERSION` bump, before/after screenshots, the manual
live-switch verification log (with `resolvectl status`), and the done-criteria
checklist.
