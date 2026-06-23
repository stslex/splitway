# Phase 7b — Tauri shell + read-only view

Stand up a Tauri 2.x desktop app (`splitway-gui-tauri`) that hosts
`splitway-gui-core` in its Rust backend and renders the core's view-model in a
thin web frontend. **Read-only**: no mutations (those are 7c); no design,
window-behavior, or packaging (those are 7d). Runs unprivileged under niri,
relying on the group-accessible socket from the socket-group phase.

This builds on 7a: gui-core already holds the socket connection, speaks the
protocol, and produces the view-model. 7b is deliberately thin — the Tauri layer
is an adapter, not a reimplementation.

## Architecture: gui-core is the brain, Tauri is a thin adapter

```
daemon socket ──(blocking JSON-lines, protocol v6)──▶ GuiCore (splitway-gui-core)
                                                          │  produces ViewModelSnapshot
            poll thread (splitway-gui-tauri/src/bridge.rs)│
                                                          ▼
   shared Mutex<ViewModelSnapshot> ──get_view_model()──▶ frontend (mount)
                                  └──"view-model-changed" event──▶ frontend (updates)
                                                          │  window.__TAURI__.event.listen
                                                          ▼
                                          render(vm): ViewModel → DOM  (ui/src/render.ts)
```

The egui harness drives the **same** `GuiCore` the same way; the only difference
is the boundary. egui renders the borrowed `GuiCore::view()` in-process each
frame; the Tauri backend serializes `GuiCore::snapshot()` (an owned, `Serialize`
mirror added in 7b) across the process/web boundary. Keeping egui working is the
proof gui-core stays framework-agnostic.

### The read path and its invariants

1. **The frontend holds no authoritative state.** `ui/src/main.ts` keeps one
   module-level `lastVm` — a *cache* of the most recent push, only ever assigned
   on the event path (or the initial fetch) and only read by `render()`. There is
   no composed/derived/optimistic state. This is what makes 7c's truth-contract
   free: a mutation can only reach the screen via daemon → VM event → render.
2. **One fat read command + Rust-side push.** `get_view_model()` returns the
   whole serialized VM, used once on mount. All later updates are
   `view-model-changed` **events carrying the full VM**, emitted by the Rust poll
   thread when the VM changes. No per-field commands; no frontend polling timers.
3. **Full VM per event, never deltas; last-wins.** Update races are benign
   (render whichever VM arrives last), and degraded states (disconnected /
   permission-denied / version-mismatch) are ordinary VM variants the frontend
   renders, not special-case code paths.
4. **The refresh loop lives in Rust.** The daemon protocol is strictly
   request/response (no server push), so *someone* must poll. `bridge::poll_loop`
   drives one whole poll cycle (`bridge::step`), publishes the snapshot to the
   shared mutex, and emits only when the snapshot **changed** (`should_emit`,
   backed by `ViewModelSnapshot: PartialEq`). `listen` is registered **before**
   the initial `invoke` so no push is missed in the mount gap.

The testable pieces (`SharedVm`, `step`, `should_emit`) are free of Tauri-runtime
coupling and unit-tested with a fake daemon. The infinite loop + `AppHandle::emit`
+ sleep is thin plumbing (not unit-tested, like the egui worker).

## Decisions

### 1. View-model scope: `Verify` only; `CheckDomain` deferred to 7c

gui-core's 7a view-model held connection / status / interfaces / config. 7b
extends it with **`Verify`** (live per-link DNS read-back + drift), surfacing the
prompt's "Link DNS state" and "Drift/verify" sections. `Verify` is added to
`GuiCore::poll()` and folded in `apply_reply`.

`CheckDomain` is **not** folded into the polled view-model. It is a *parameterized
one-shot query* (raw user input — a pasted host), not ambient system state.
Auto-issuing it per configured domain every poll would mean N live DNS
resolutions per cycle — poll latency becomes the sum of N resolutions, and one
slow/timing-out domain stalls the whole VM refresh — and it overloads the verb's
semantics before 7c gives it its natural interactive home (a query that returns
its own result on demand). **The boundary: ambient system state belongs in the
polled VM; parameterized queries are interactive request/response in 7c.**

Three properties the implementation guarantees:

- **Same-cycle coherence.** gui-core stores the live read-back *raw*
  (`VerifyState::Live(LinkDnsState)`); the drift verdict is computed at
  `snapshot()` time via the shared `compare_drift(&live, status.applied)` — i.e.
  against the `applied` belief carried in the *same* snapshot. A driver that
  drains a whole poll cycle before snapshotting therefore never pairs one cycle's
  belief with another's reality. (`status` and `verify` are both fetched in the
  cycle the snapshot is taken from.)
- **Isolated degradation.** A failed `Verify` round-trip turns the verify section
  into `VerifyView::Unavailable` and touches nothing else — the connection banner
  is owned by the `Status` poll alongside it, so a Verify error never alters
  `connection`/`last_health`. The rest of the snapshot stays valid.
- **`NotApplicable` is first-class.** A healthy "nothing is applied, so there is
  nothing to compare" (`DriftVerdict::NotApplicable`) rides inside
  `VerifyView::Available`, distinct from `Unavailable` (a transport failure).

**No protocol bump.** `Verify` already exists at v6; 7b consumes existing read
verbs only.

### 2. Frontend: vanilla TypeScript, single `render(vm)`

No UI framework. `render(vm, root)` clears and rebuilds the DOM from the VM each
call. The reason is structural, not just cost: `render(vm)` has nowhere to hold
authoritative state, which enforces invariant 1 directly and makes it greppable
(no state assignment outside the VM-event path). DOM is built with
`createElement` + `textContent` (never `innerHTML` with interpolated daemon
strings), so a domain name or error message from the daemon cannot inject markup.

### 3. Type-sharing: hand-written TS mirror + a layered drift guard

The VM is ~12 fields, so the TS mirror (`ui/src/bindings/view-model.ts`) is
hand-written rather than generated (no specta ↔ Tauri-2 version gamble for a type
7d will likely reshape). Three guards keep it honest:

- **Rust serialize-vs-fixture** (`bridge.rs` test, the authoritative shape lock):
  a representative snapshot must serialize to exactly the committed
  `ui/src/bindings/view-model.sample.json`. Any change to the Rust shape (a
  renamed/added/removed field, a changed enum repr) fails this test. Regenerate
  with `UPDATE_VIEW_MODEL_FIXTURE=1 cargo test -p splitway-gui-tauri --lib`.
- **gui-core serde-shape** test (runs in CI / `nix flake check`): asserts the
  top-level keys + the `verify` internal tag + the externally-tagged `drift`
  shape directly on `ViewModelSnapshot`.
- **TS compile-time** (`ui/src/contract-check.ts`, checked by `tsc --noEmit`):
  the committed fixture must have every top-level key the mirror declares, and
  `render` must accept a `ViewModel`. (A direct `const x: ViewModel = fixture`
  can't be used: `resolveJsonModule` widens the JSON's enum strings to `string`,
  so the literal-union fields would falsely fail; the top-level key check is
  widening-safe, and deep/enum shape is locked by the two Rust guards above.)

Serde shapes the TS mirror reproduces: unit-only enums (`Health`, `RoutingState`)
→ bare strings; data enums externally tagged (`DetectorHealth::Error` →
`{"Error": s}`, `DriftVerdict::Drifted` → `{"Drifted": {...}}`); `VpnBackend`
kebab-case; `VerifyView` internally tagged on `state` (a Tauri-side type we own,
chosen for an ergonomic discriminated union).

### 4. niri / webkit2gtk blank-window workaround

Tauri on Linux uses webkit2gtk (4.1) via wry. Its DMA-BUF renderer can fail to
initialise under Wayland compositors and then **silently render a blank window**.
The fix (Tauri's own Linux-graphics guide) is `WEBKIT_DISABLE_DMABUF_RENDERER=1`,
set in `main.rs` **before** the Tauri builder (it must precede GTK init). This box
is not NVIDIA, so no `__NV_*` vars are needed; `WEBKIT_DISABLE_COMPOSITING_MODE`
is a documented last-resort for a different symptom (resize crashes) and is not
set. **Verified empirically on niri**: the window renders the read-only view (see
Verification).

### 5. Capabilities + CSP (read-only, minimal)

`capabilities/default.json` grants the `main` window `core:default` (so its
custom command is reachable over IPC) + `core:event:default` (so the frontend may
`listen`). Custom commands need no extra ACL permission in Tauri 2. No fs / shell
/ http permissions. The CSP is minimal and self-only plus the Tauri IPC origins
(`ipc:`, `http://ipc.localhost`).

## Build & toolchain reality (NixOS), and what is deferred to 7d

`splitway-gui-tauri` links webkit2gtk and embeds a pre-built web frontend, so it
is **excluded from the workspace `default-members`**: `cargo build` / `cargo test`
— and the `nix build` / `nix flake check` that wrap them — operate on the
default set (daemon/CLI/shared/gui-core/egui), unchanged, with no webkit
toolchain. The Tauri crate is built locally inside `nix develop` (the dev shell
gained `webkitgtk_4_1`, `gtk3`, …, `pkg-config`, plus `nodejs`, `typescript`,
`esbuild`, and the runtime GIO/gsettings/DMABUF env). **Packaging it for Nix
(frontend vendoring + `wrapGAppsHook3` + bundling) is Phase 7d.**

Two environment-forced toolchain substitutions (this box reaches the network only
through a proxy that npm could not pull tarballs through; the TypeScript source is
unchanged either way, and both are reversible in 7d):

- **esbuild + `tsc` (from Nix) instead of Vite** to type-check and bundle the
  frontend (`ui/build.sh`). No npm registry dependency.
- **`withGlobalTauri` instead of the `@tauri-apps/api` npm package**: `invoke` /
  `listen` come from `window.__TAURI__` (typed in `ui/src/tauri-global.d.ts`), so
  the runtime has no npm dependency.

A Vitest render smoke test was planned but `jsdom`'s dependency tree could not be
fetched through the proxy; the runtime render check is deferred (render() is
exercised for real in the e2e below, and the contract is locked by the Rust-side
guards + the `tsc` compile-time guard).

### Local build / run

```sh
nix develop            # provides webkit, pkg-config, tsc, esbuild, runtime env
sh splitway-gui-tauri/ui/build.sh         # → ui/dist (tsc check + esbuild bundle)
cargo build -p splitway-gui-tauri         # embeds ui/dist via generate_context!
./target/debug/splitway-gui-tauri         # runs against the daemon socket
```

`run()` (the only `generate_context!` user, which needs `ui/dist`) is gated
`#[cfg(not(test))]`, so `cargo test -p splitway-gui-tauri --lib` exercises the
bridge + regenerates the fixture without first building the frontend.

## Verification (what was proven)

- **Builds + renders under niri** — the empirical blank-window check. The window
  renders the read-only view (screenshotted); no webkit/DMABUF errors, no panic.
- **Disconnected variant** renders cleanly: "● Daemon not running" with the
  verbatim socket guidance, and every section shows its unavailable placeholder.
- **Connected + Verify** (against a user-space `enabled:false` daemon — never
  applies DNS, so a live VPN is untouched): "● Connected", status, routed
  domains, real interfaces enumerated, and `Verify` Available with drift
  `not applicable (nothing applied)`.
- **Live config-edit end-to-end** (the read-path proof, 7c's foundation): editing
  the config file on disk → daemon inotify reload → GUI poll → `view-model-changed`
  → the new domain appears in the UI with no manual refresh.
- **Unit**: `bridge` tests (fake daemon → connected/disconnected snapshots,
  emit-on-change, fixture guard) and gui-core's Verify/snapshot/coherence tests.
- egui harness still builds; `cargo fmt/build/test/clippy` green (default-members
  and the Tauri crate explicitly); no protocol version change.

## Explicitly out of scope (later phases)

- Any mutation / write path, and the interactive `CheckDomain` query — **7c**.
- Visual design, window behavior, niri window rules, Nix packaging/bundling,
  restoring Vite/specta if desired — **7d**.
- macOS socket access + packaging; SO_PEERCRED / auth; multi-VPN / multi-profile.
- Permission-denied and version-mismatch are ordinary VM variants rendered by the
  same path as the verified disconnected variant (and covered by gui-core's
  `Health` unit tests); they were not separately screenshotted.
