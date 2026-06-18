# Phase 5 — GUI usability: live config, interface selection, polish

Implement Phase 5 from `ROADMAP.md`. Read `CLAUDE.md` first. Phase 4 shipped a
working-but-rough GUI (`splitway-gui`); this phase makes it correct and
presentable. Four user-visible goals, plus the daemon support two of them need:

1. **Config changes take effect live.** Editing `vpn_name` / `vpn_backend` /
   `openvpn.*` must re-point auto-apply without a daemon restart, and `vpn up`
   must then reflect the configured interface — today the detector watch is armed
   once at startup, so `vpn up` tracks the *startup* interface and a freshly-typed
   `vpn_name` does nothing until restart.
2. **Pick the interface from a list.** The user should choose `vpn_name` from the
   currently-present interfaces (with up/down shown), not type a guess.
3. **A real Resync button** to force the daemon to re-read + reconcile and refresh
   the view.
4. **The window should look presentable.** The current build renders with a
   transparent background and cramped, unstyled widgets.

The GUI stays what Phase 4 made it: a **pure IPC client — zero privileges, zero
duplicated logic, writes no files itself**. The two new capabilities (live
re-arm, interface enumeration) therefore live in the daemon and are reached over
IPC.

## Branch

Branch `phase-5-gui-usability` from up-to-date `dev` (after the Phase 4 PR has
merged — this builds directly on it).

## The core problem (read before designing)

Read `splitway-daemon/src/daemon/mod.rs` (`run_async`) and
`splitway-daemon/src/daemon/state.rs`. Today the watch is wired once:
`create_vpn_detector(&config).watch(&interface)` is called in `run_async` at
startup, its event stream is forwarded into the state actor's `state_tx`, and the
actor's config can change afterwards (`SetConfig`/`ReloadConfig`) **without the
watch ever being rebuilt**. `state.rs::warn_on_restart_only_changes` exists
precisely because of this — it logs "takes effect after a daemon restart" instead
of acting. So `StatusInfo.vpn_up` is driven only by events on the startup
interface; after editing `vpn_name`, `desired()` sees `interface_name !=
config.vpn_name`, reverts the old interface, and can never apply to the new one.
That is the root of the "vpn_up looks wrong / isn't tied to the GUI's vpn_name"
report.

Making config edits live therefore means giving the detector watch a **dynamic
lifecycle** — start, stop, restart — driven by config changes, instead of a
one-shot at boot. This is the central design task; the interface list and the GUI
work are comparatively mechanical.

## Design decision A — live re-arm of the VPN watch (justify in the PR)

The detectors themselves are **reused unchanged** (NM, standalone OpenVPN,
macOS); only *when* a watch starts/stops changes. The seam is the question: the
state actor owns the config and learns of changes, but the watch task is spawned
in `run_async`.

- **Recommended:** give the state actor ownership of the watch lifecycle. It
  holds a clone of `state_tx` (to feed new watch tasks) and an
  `Option<tokio::task::AbortHandle>` (or a per-watch cancel `oneshot`/token) for
  the current watch. Arm once at startup as today. In `commit()` / `reload_config()`,
  after the new config is adopted, if `vpn_name` / `vpn_backend` / `openvpn`
  changed, **re-arm**: abort/stop the old watch, reset `vpn_up = false` and
  `last_info = None`, `reconcile()` (which now reverts the old interface because
  `desired()` no longer matches), then build `create_vpn_detector(&new_config)`
  and spawn a fresh watch on the new `vpn_name`. Replace
  `warn_on_restart_only_changes` with this re-arm path (the warning becomes
  action). An empty new `vpn_name` tears the watch down and leaves none running
  (the startup-empty behavior).
- **Sample immediately on re-arm.** After arming the stream, call the detector's
  one-shot `detect()` once and feed its result, so `vpn_up` reflects the new
  interface's *current* state without waiting for the next up/down edge. Dedup the
  one-shot sample against the first streamed event (the detectors already use a
  `Deduper`/"sample after arming" pattern — mirror it).
- **Cancellation must actually stop the detector.** Aborting the watch task must
  release the detector's resources (the NM D-Bus connection, the OpenVPN
  management socket). The detectors stop on `tx.closed()` / `tokio::select!`;
  confirm that dropping/aborting the spawned task drops those resources, and if a
  detector needs explicit teardown, give the watch task a cancel branch rather
  than a bare `abort()`. Document what you relied on.
- **Rejected:** keeping the watch in `run_async` and poking it from the actor via
  a side channel. It splits the lifecycle across two owners and races config
  adoption against re-arm. Keep config change and re-arm in one place (the actor).

This removes the restart caveat, so the GUI's restart warnings and
`model::interface_change_needs_restart` must go (they would now be false). Keep
the existing "no half-configured state" guarantee: the old interface is reverted
before/inside the re-arm reconcile, and a re-arm whose new detector fails to
start must leave IPC up with auto-apply off and a clear error/log — never a
half-applied or panicking daemon (mirror `run_async`'s current "watch failed"
handling).

## Design decision B — `ListInterfaces` over IPC

Add a read-only verb so the GUI can offer an interface picker without touching the
platform or holding privileges (per the roadmap mandate).

- `Request::ListInterfaces -> Response::Interfaces(Vec<InterfaceInfo>)`, with
  `InterfaceInfo { name: String, up: bool, vpn_like: bool }` (a small dedicated
  wire type in `ipc.rs`, like `ConfigView`). `vpn_like` is a name-prefix heuristic
  (`tun` / `utun` / `wg` / `tap` / `ppp` / `gpd*`) so the GUI can sort/flag likely
  VPN devices; it is advisory, not a filter.
- **Enumeration** is unprivileged and read-only: a thin per-platform layer (Linux:
  `/sys/class/net/<n>/operstate` + flags, or `getifaddrs`; macOS: `getifaddrs`).
  Keep the platform I/O thin and untested; the **classification + sort/dedup**
  (`vpn_like`, ordering up-first then vpn-like-first, de-duplicating the multiple
  `getifaddrs` entries per interface) is pure and unit-tested with fixtures.
- Loopback is included but `vpn_like = false`; let the GUI present/sort, don't
  filter in the daemon.
- **Bump `PROTOCOL_VERSION` to 3.** Additive variants only; the existing
  version-peek + strict-equality handling (`daemon::ipc::process_line`,
  `VERSION_MISMATCH_PREFIX`) already covers skew — daemon/CLI/GUI upgrade in
  lockstep. Round-trip + back-compat tests for the new wire types, as for
  `ConfigView`.
- A CLI `splitway interfaces` command is **optional** and may be skipped to keep
  the CLI minimal — note the choice.

## GUI changes

- **Interface picker.** `vpn_name` becomes a combo populated from
  `ListInterfaces` (up first, `vpn_like` flagged), refreshed on connect, on the
  poll, and after Resync. Keep a free-text fallback and always preserve the
  currently-configured value even when it is absent/down (it may be a VPN that is
  not up right now). Selecting an interface is a normal edit saved via
  `SetConfig`.
- **`vpn up` clarity.** With live re-arm, `vpn_up` now tracks the configured
  interface — show it plainly next to that interface, and drop the stale
  "needs a restart" notes. Optionally show the picked interface's own up/down from
  `ListInterfaces` so the state is unambiguous.
- **Resync button** (header, near the connection banner): send
  `ReloadConfig` (daemon re-reads the active file from disk, reconciles, and
  re-arms if the on-disk `vpn_name`/backend differs) and then refetch
  `Status` + `GetConfig` + `ListInterfaces`. Define this in the PR. It refreshes
  the *daemon's* truth, so it intentionally discards unsaved editor buffers only
  after confirming, or leaves them per the existing unsaved-edit guard — pick one
  and say which.
- **Auto-refresh on change.** After every successful mutation (enable/disable,
  add/remove, save config, resync) immediately refetch `Status` + `GetConfig`
  (and `ListInterfaces` where relevant) rather than waiting for the next poll, so
  the view never lags a change the user just made. Keep the periodic poll as the
  backstop.
- **Visual pass (concrete).** The transparent/cramped look comes from rendering
  into the bare root `ui()` with no panel background. Fix it directly:
  - Render inside an opaque `egui::CentralPanel` (and a top region for the
    header/connection/Resync) instead of the background-less root `Ui`.
  - Group each section (Status / Domains / Configuration / Config file) in a
    bordered `egui::Frame`/group with consistent inner margins and section
    spacing; align label/field columns with a `Grid`; constrain text-field widths
    instead of full-bleed stretch.
  - Set a consistent dark visual style and legible colors (the current
    green-on-transparent is hard to read), a comfortable default window size, and
    keep the whole thing inside the existing vertical `ScrollArea`.
  - This is polish, not a redesign — stay egui-idiomatic and keep the rendering as
    untested plumbing; new decision logic still goes in `model.rs` with tests.

## Failure modes (handle; test the pure parts)

- **Re-arm to a missing/never-up interface.** Watch starts (or `detect()` returns
  down); `vpn_up = false`; no apply. No error spam.
- **Re-arm where the new detector cannot start** (NM absent, bad `openvpn.management`).
  Auto-apply off, IPC stays up, clear error to the client + log; daemon does not
  crash and does not strand the old interface's rules (revert first).
- **Rapid successive config changes.** Each re-arm cancels the previous watch
  before starting the next; no leaked detector tasks/sockets; final state matches
  the final config.
- **`ListInterfaces` enumeration error** (e.g. sysfs read fails): return a clear
  `Response::Error`, not a panic; the GUI shows the picker as unavailable and
  falls back to free-text `vpn_name`.
- **Resync while the daemon is down / version-skewed**: reuse the existing
  `ClientError` / version-mismatch banners; Resync is just requests.

## Out of scope

- Changing detection *logic* in any detector (re-arm reuses them as-is) or any new
  privileged operation in the GUI.
- Tray icon, notifications, autostart, per-domain live status, full theming, rule
  editing beyond the domain list.
- Packaging/distribution — that is now **Phase 6**; wrapping the `nix build` GUI
  binary with its runtime library path (so `./result/bin/splitway-gui` finds
  `libGL`/`wayland`/`libxkbcommon`) belongs there, noted as a follow-up.
- Windows GUI; proxy/route targets.

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macOS); the Linux GL deps and flake
  inputs from Phase 4 already cover the build.
- On a live daemon (Linux): changing `vpn_name` between two interfaces in the GUI
  re-points auto-apply **with no restart**, `vpn up` tracks the configured
  interface, and the old interface is reverted on switch (no half-configured
  state). Manual log + before/after `resolvectl status` in the PR.
- The interface picker lists current interfaces (up flagged) and selecting one
  saves via `SetConfig`; a down/absent configured interface is still shown.
- Resync re-reads + reconciles + refreshes the view; every mutation refreshes the
  view immediately.
- Visual: before/after screenshots in the PR; the window has an opaque background,
  grouped sections, aligned fields, legible text.
- `PROTOCOL_VERSION` bumped to 3, additive; new wire types round-trip; skew still
  surfaced as "update splitway". Daemon/CLI/GUI in lockstep.
- Pure logic unit-tested: re-arm decision ("does this config delta require a
  re-arm?"), interface classification/sort/dedup, the GUI model additions
  (picker selection, resync action, vpn_up display). Daemon re-arm covered with a
  mock detector (old watch stopped, new armed, `vpn_up` reset/sampled, old
  interface reverted, arm-failure keeps IPC up).
- `README.md` updated: the GUI now applies config changes live (drop the
  restart caveat), offers an interface picker, and has a Resync button.
- No regression in existing daemon/CLI/GUI tests.

## Finish

PR into `dev` titled `Phase 5: GUI usability (live config, interface selection,
polish)`. Description: the re-arm lifecycle design (the actor-owned watch + the
cancellation/teardown you relied on), the `ListInterfaces` design, the Resync
semantics and the unsaved-edit decision, the `PROTOCOL_VERSION` bump, before/after
screenshots, the manual live-switch verification log, the done-criteria checklist,
and the noted Phase 6 follow-up (wrapping the packaged GUI binary's library path).
