# Phase 7d (part 1) — visual design + window behavior

Turn the read/write Tauri shell from 7b/7c into the approved **Variant B** design:
a full top-level Wayland window that renders the daemon's view-model through the
mockup's layout, with the simplified interface-centric model and a stable niri
`app_id`. The truth contract from 7c is preserved.

**Bundling is split out into 7d-2** (Nix packaging, distribution icons, `.desktop`,
flake `packages`, README GUI-install section). The seam is clean — design+window
is fully buildable/testable in-tree; bundling's real proof (the *built* binary
rendering for a fresh niri user) is machine-bound and belongs in a focused PR.

Visual source of truth: the committed mockups under `docs/design/mockups/`
(`splitway-mockup-variant-b.html`, `splitway-states.html`). Their preview-state
switcher and synthetic data are mockup-only and were **not** ported — real state
comes from the view-model.

## Confirm-first outcomes (spec §"Confirm first", checked against the code)

1. **Interface-centric / DNS-auto model — already true.** The daemon detects the
   VPN's DNS from the configured interface (`VpnDetector` → `VpnInfo { interface,
   dns_servers }`) and applies via `resolvectl dns/domain <iface>`; there is no
   explicit-DNS config field. The detected DNS for the selected interface lived in
   the daemon's `last_info` but was only surfaced via `StatusInfo.applied` (present
   only once routing is *applied*). The mockup shows that DNS read-only even when
   nothing is applied (empty/disabled), so this PR adds the one authorized
   prerequisite (below).
2. **OpenVPN management socket — load-bearing, but hiding the GUI fields is safe.**
   The standalone-OpenVPN detector does real work for `vpn_backend = openvpn`
   users. The Variant B design removes the backend/OpenVPN fields and the settings
   screen *from the GUI only* — the daemon keeps parsing and using every config
   field. Safety hinges on the guardrail below.
3. **Blank-window workaround — recorded.** `WEBKIT_DISABLE_DMABUF_RENDERER=1`
   (set in `main.rs` before GTK init) is documented in `tauri-read-only.md` §4.
   Baking it into the packaged wrapper is a 7d-2 task.
4. **Wayland `app_id` — was the binary name (`splitway-gui-tauri`), now stable.**
   Tauri 2.x only sets the GTK/Wayland app_id to the bundle `identifier` when
   `app.enableGTKAppId` is true (verified in tauri-utils 2.9.3,
   `AppConfig::enable_gtk_app_id`). Set it → app_id = `io.github.stslex.splitway`.
   The 7d-2 `.desktop` `StartupWMClass` must match this string.
5. **Nix Tauri packaging — shape confirmed for 7d-2** (rustPlatform.buildRustPackage
   + `wrapGAppsHook3` + `webkitgtk_4_1` + `makeWrapper`, frontend built in-derivation
   via `ui/build.sh` — no npm; `librsvg`/`rsvg-convert` for icon PNGs; bundle the
   workaround via `wrapProgram`; `bundle.active = false`).

## The simplified data model (governs the whole UI)

Two inputs: **interface + domains.** Nothing else.

- **DNS is auto-derived from the selected interface, shown read-only.** Manual DNS
  entry is **not** a field. The mockup's "DNS not detected" state shows a manual
  input as the fix; the daemon has no manual-DNS override (no config field, no
  verb), so a manual-entry box would be a dead/fake control and would violate the
  truth contract. That state is rendered **informationally** instead (amber warning
  pointing at the real fixes: pick the VPN's interface, or check that the VPN
  pushes DNS). **Manual-DNS-override is a legitimate FUTURE daemon feature** (config
  field + routing logic + verb) for VPNs that connect but push no DNS to resolved —
  it is deferred, not deleted; the mockup's manual input represents that future
  state. (Decision confirmed with the author.)
- **No vpn-name / backend fields, no settings screen.** The interface selector
  ("Route DNS through") *is* the `vpn_name` writer.

### Guardrail: hiding backend/OpenVPN is safe only by round-trip

The interface selector is the GUI's **only** config writer. A `SetConfig` is a full
update of `{vpn_name, vpn_backend, openvpn_management, openvpn_management_password_file}`
(the daemon stores what it is sent), so a write that omitted or defaulted a hidden
field would silently reset an OpenVPN user's backend/endpoint. `config-input.ts`
(`configInputForInterface`) builds the payload from the daemon's **current**
`config` and changes only `vpn_name` — a read-modify-write that preserves the hidden
fields by construction. Covered by `ui/test/config-input.test.ts` (run via
`ui/test.sh`: esbuild → node, no jsdom).

## The one authorized daemon change: `StatusInfo.detected_dns` (protocol v6 → v7)

Additive read-path field exposing the configured interface's *detected* DNS
independent of apply state, sourced from `last_info` (gated on
`last_info.interface_name == config.vpn_name`, the same guard `desired()`/
`routing_state()` use). Empty when no interface / down / no DNS pushed. Done in
lockstep across `splitway-shared` (the wire type + `PROTOCOL_VERSION` 6→7),
`splitway-daemon` (`status()` + the CLI/daemon `status` printers), `splitway-gui-core`
(passes `StatusInfo` through unchanged), and `splitway-gui-tauri` (TS mirror +
regenerated `view-model.sample.json`). This is the **one** intentional protocol bump
this phase (everything else is presentation). egui reads `StatusInfo` fields and is
untouched.

## Layout & states

Top→bottom per the mockup: topbar (inline brand **mark** + wordmark + connection
indicator) → hero (pitch + ON/OFF toggle + status line + chip) → interface block
(selector + DNS readout / informational not-detected fix) → domains (eyebrow with
count + Add; scrollable card list with route badge, verify ✓/⚠, delete-on-hover;
`Everything else → direct` below) → check a domain → footer.

Every view-model variant has a designed state, derived purely (`render.ts:stageFor`):

| VM condition | Rendered state |
| --- | --- |
| `Connected` + enabled + `Applied` | healthy (per-domain ✓/⚠ from `verify` drift) |
| enabled + `VpnDown` | waiting — amber, domains dimmed |
| enabled + `NoDnsFromVpn` | DNS-not-detected — informational amber fix |
| enabled + `NoDomains` | empty — invite to add |
| `!enabled` (`Disabled`) | off — routing paused |
| enabled + `ApplyFailed` | out-of-sync — Resync affordance |
| `NotRunning` / `TransientError` | full-window blocker "Can't reach Splitway" |
| `PermissionDenied` | full-window blocker "No permission" (group fix) |
| `ConfigInvalid` | full-window blocker "Configuration can't be loaded" |
| `VersionMismatch` | full-window blocker "Update needed" (daemon's guidance) |

## Interactions through the truth contract (7c, preserved)

- **Check** — disabled + spinner while the daemon answers `check_domain`; the
  result is ephemeral (lifecycle store), never folded into the VM.
- **Delete + undo** — delete is a daemon write; the undo snackbar is ephemeral UX
  over the *completed* delete (~11s window); Undo is a re-add daemon write. The
  displayed list always comes from the VM after each write.
- Domain/interface/status state renders **only** from `view-model-changed`. The
  greppable invariants still hold after the redesign: `lastVm` is assigned only in
  `applyVm`; DOM is built with `createElement`/`createElementNS` + `textContent`
  (a `grep -rn innerHTML ui/src` returns nothing — SVG icons use `createElementNS`).

## Rendering model note (animations vs. full rebuild)

The frontend re-renders by rebuilding `#app`'s children on every VM event/keystroke
(this is what makes "no held state" greppable). CSS transitions/animations keyed to
element creation would replay (flash) on each rebuild. So:

- The **staggered entrance reveal** plays **once**: it is gated on an `.intro` class
  the controller adds to the *persistent* `#app` root on the first main paint and
  removes ~900 ms later; rebuilt children find no `.intro` and appear instantly.
- **Hover / focus** transitions and the **spinner** work as-is (pseudo-class /
  continuous keyframe, independent of rebuild).
- The toggle and undo-snackbar are styled with transitions, but because the
  controls are rebuilt they settle instantly rather than springing — clean and
  on-brand (monotone, no glow), and consistent with "restrained motion" +
  `prefers-reduced-motion`.

## Window behavior

- Full top-level window (no tray — niri has none). Stable `app_id`
  `io.github.stslex.splitway` via `enableGTKAppId`.
- Default 560×820, min 380×480; single-column layout capped at 680 px and centered,
  reflowing to narrow niri columns (the ≤380 px query hides the pitch + connection
  label). The domains list scrolls internally; the window scrolls for short tiles.
- In-app brand **mark** (bare) inline in the topbar; the tiled icon is distribution-only (7d-2).

## Fonts

IBM Plex Sans (human) / IBM Plex Mono (machine) via CSS stacks with platform
fallbacks (`system-ui` / `ui-monospace`) — no webfont CDN (the CSP is self-only), so
the machine→human typographic split holds even before the OFL `woff2` files are
bundled. Bundling the `woff2` + `@font-face` lands with packaging in **7d-2**.

## Verification

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D warnings`,
  `cargo test --workspace` — all green (incl. `splitway-gui-tauri`; egui still
  builds). `ui/build.sh` (tsc typecheck incl. the bindings drift guard + esbuild)
  and `ui/test.sh` (config-input guardrail) pass; the `view-model.sample.json`
  fixture was regenerated and matches.
- **Deferred to the author (machine-bound):** the on-screen niri render check
  (`nix develop` → `sh ui/build.sh` → `cargo run -p splitway-gui-tauri`). The render
  pipeline is unchanged from 7b (empirically verified on niri there), and the
  monotone design removed any `backdrop-filter` dependency, so only the documented
  blank-window workaround matters.

## Out of scope (→ 7d-2 or later)

Nix bundling / icons / `.desktop` / flake `packages` / README GUI-install (7d-2);
bundling the IBM Plex `woff2` (7d-2); the manual-DNS-override daemon feature
(future); retiring the egui reference.
