# Phase 4 — Primitive GUI

Implement Phase 4 from `ROADMAP.md`. Read `CLAUDE.md` first. Scope: a small
desktop GUI that lets a user **enable/disable** rule application, **see the
current status**, **select the config file** the daemon uses, and **edit that
config** — all through the existing daemon IPC socket, holding no privileges and
duplicating no daemon logic.

This is the roadmap's "primitive GUI": a thin front-end over the same Unix
socket the CLI already drives. The headline constraint (`ROADMAP.md`, Phase 4)
is **zero duplicated logic, zero privileges in the GUI process** — the GUI never
touches `resolvectl`/`/etc/resolver`, never writes the config file itself, and
never escalates. It is, like `splitway-cli`, a client of `splitway-shared::ipc`.

## Branch

Branch `phase-4-gui` from up-to-date `dev`.

## The core problem (read before designing)

The roadmap asks for "config file selection + editing **over IPC** with zero
duplicated logic." The current IPC surface cannot express that yet. Read
`splitway-shared/src/ipc.rs` and `splitway-daemon/src/daemon/state.rs`:

- `Request` today is `Status | Enable | Disable | AddDomain | RemoveDomain |
  ListDomains | ReloadConfig`. `Response` is `Ok | Status(StatusInfo) |
  Domains(Vec<String>) | Error(String)`.
- `StatusInfo` exposes `enabled, interface, vpn_up, applied, domains` — read-only,
  and only a subset of `LocalConfig`.
- There is **no** verb to read the full config (`vpn_name`, `vpn_backend`,
  `openvpn.*`), **no** verb to set any field other than the domain list and
  `enabled`, and **no** notion of which config *file* is in effect (the daemon
  computes a single fixed path via `config::config_file_path()` at startup and
  the state actor only ever re-reads *that* path on `ReloadConfig`).

So three of the four requested capabilities already fit the protocol — the
toggle (`Enable`/`Disable`), the status view (`Status`), and domain editing
(`AddDomain`/`RemoveDomain`/`ListDomains`) — but **full-config editing** and
**config-file selection** do not. The "zero duplicated logic" mandate rules out
the obvious shortcut (have the GUI read/write the JSON file directly and nudge
the daemon with `ReloadConfig`): that makes the GUI a *second writer* of a file
the daemon's `commit()` also writes, racing lost updates, and it simply cannot
work for a root system-service daemon whose config and socket the unprivileged
GUI cannot touch. The clean path is therefore a **minimal IPC extension** so the
daemon stays the single writer and the GUI stays a pure client. Designing that
extension is the main engineering task of this phase; the egui front-end is
mechanical once the protocol is right.

## Investigate / confirm first (record findings in the PR)

1. **Privilege/socket reachability.** Confirm the GUI's reachability matches the
   CLI's: `ipc::client::send_request` tries the per-user socket then the system
   socket, and a root daemon's `0600` socket yields `ClientError::PermissionDenied`
   to an unprivileged client (`daemon/ipc.rs` `bind_socket`). Decide and document
   how the GUI presents each `ClientError` variant (`NotRunning`, `PermissionDenied`,
   `Io`, `Protocol`) — these already carry actionable messages; reuse them, do not
   re-word the security guidance.
2. **Blocking client on a UI thread.** `client::send_request` is a *synchronous,
   blocking* `UnixStream` round-trip. egui/eframe runs its event loop on the main
   thread; calling the client there freezes the UI. Confirm the worker-thread
   design below and that no tokio runtime is pulled into the GUI.
3. **`vpn_name` change semantics.** Re-confirm from `state.rs` (`reload_config`,
   `desired`) that changing `vpn_name` reverts the old interface but does **not**
   re-arm the detector watch — auto-apply for the new interface needs a daemon
   restart. The GUI must surface this caveat when the user edits `vpn_name`, not
   silently imply it took full effect.
4. **What "config file selection" must mean.** The daemon today binds one fixed
   config path for its whole lifetime. Decide (design section) whether v1
   selection means *editing the daemon's active file* + showing its path, or also
   *switching* the daemon to a different file at runtime — and if the latter, what
   that costs (a new verb + path validation + the same `vpn_name`/watch caveat).

## GUI design

Add a new workspace member `splitway-gui` (binary `splitway-gui`), added to the
root `Cargo.toml` `members`. Stack: `eframe`/`egui` (roadmap-pinned: pure Rust,
Linux+macOS, fastest to ship). It depends on `splitway-shared` for the IPC
client and types, and on nothing in `splitway-daemon`.

- **Pure client, no logic.** Build every action as a `Request` and render every
  `Response`, exactly as `splitway-cli/src/main.rs` does. No DNS, no config-file
  writing, no privilege, no daemon types beyond `splitway-shared::ipc`.
- **Threading (correctness + responsiveness).** Keep the blocking IPC off the UI
  thread. Spawn one worker `std::thread` that owns a `std::sync::mpsc` request
  queue, calls `client::send_request`, and posts `Result<Response, ClientError>`
  back over a channel; the UI thread drains replies each frame and calls
  `egui::Context::request_repaint` (or `request_repaint_after`) to wake. No
  tokio. Keep at most one request in flight; show a pending indicator while it is.
- **Status polling.** Poll `Request::Status` on a timer (e.g. every 1–2 s) plus
  on demand after any mutating action, so the toggle/applied/vpn_up display stays
  live without a push channel (the protocol has none). A poll that returns
  `NotRunning` must degrade gracefully to a clear "daemon not running" state, not
  spin or crash, and must recover when the daemon returns.
- **Layout (primitive — nothing more):** an enable/disable toggle bound to
  `Enable`/`Disable`; a read-only status block (`vpn_up`, `applied`, `interface`,
  domain count); a domain list with add/remove; the config-file path with a
  native file picker (e.g. `rfd`) for selection; and an editor for the remaining
  editable fields (`vpn_name`, `vpn_backend`, `openvpn.management`,
  `openvpn.management_password_file`) saved via the config verb decided below.
  No tray icon, no notifications, no per-domain live status, no theming.

Mirror the repo convention of separating **pure logic** (unit-tested) from
**thin plumbing** (not unit-tested) — the same split as `detector/.../parser.rs`
vs `mgmt.rs`, or macOS `state.rs` vs `watch.rs`. Put the view-model reducer
(map `StatusInfo` + last error → what the widgets show; validate a domain string
before sending; categorize a `ClientError` into a user-facing message) in a pure
module with tests; keep egui rendering and the worker thread as untested
plumbing.

## Design decision — config editing & file selection over IPC (justify in the PR)

The protocol gap from "the core problem" must be closed so the GUI stays a pure
client. Recommended, in order:

- **Add a read + write verb pair for the full editable config.** e.g.
  `Request::GetConfig -> Response::Config(ConfigView)` and
  `Request::SetConfig(ConfigView) -> Ok/Error`, where `ConfigView` is the
  editable projection of `LocalConfig` (deliberately **not** re-exporting
  `LocalConfig` from `splitway-shared`'s config module into the wire type if that
  couples the wire format to on-disk serde; a small dedicated struct in `ipc.rs`
  is cleaner and versionable). The daemon handles `SetConfig` in the **state
  actor** (`StateMachine`), reusing the existing `commit()` path so it remains the
  single writer: persist atomically via `save_config_to`, adopt in memory, then
  `reconcile()` — identical safety to `AddDomain`/`Enable` today. This also lets a
  future CLI `set`/`get` reuse the same verbs (no GUI-only surface).
- **Bump `PROTOCOL_VERSION` to 2.** Note the current rule is *strict equality*:
  `process_line` rejects any envelope whose `version != PROTOCOL_VERSION`. So a v2
  daemon rejects a v1 client and vice-versa — there is no silent mixed-version
  operation, and because the daemon, CLI and GUI all build from this one
  workspace they upgrade in lockstep. Keep that strict check (it fails loud, not
  silent); the work is to render the resulting mismatch `Response::Error` in the
  GUI/CLI as actionable "update splitway" guidance, not a raw error. If
  mixed-version operation is ever wanted, the alternative is a min-supported-version
  range instead of equality — out of scope here unless investigation forces it.
  New `Request`/`Response` variants are additive in the enums; verify every variant
  still round-trips (extend the existing `envelope_round_trip_carries_version` /
  `response_round_trip` tests).
- **Config-file selection.** Recommended for v1: surface the daemon's *effective*
  config path (add it to `ConfigView` or `StatusInfo`) and let the GUI open/edit
  that file's contents via `GetConfig`/`SetConfig`; back the file picker with a
  daemon `--config <PATH>` startup override (thread an `Option<PathBuf>` through
  the single `config_file_path()` call site) so the operator chooses the file at
  launch. **Runtime switching** of the daemon's active file from the GUI (a
  `LoadConfigFrom(path)` verb) is the tempting but heavier option: it needs path
  validation (the daemon must not be coerced into reading arbitrary paths — refuse
  for the system/root daemon or confine to the user's config dir) and carries the
  same `vpn_name`→watch-restart caveat. **Recommended: defer runtime switching to
  a follow-up** and keep v1 to "edit the active config + choose it at launch,"
  unless the investigation shows runtime switching is cheap and safe. State which
  you chose and why in the PR.
- **Rejected:** GUI writes `config.json` directly + `ReloadConfig`. Two writers
  to one file (the daemon's `commit()` also writes it) races lost updates,
  duplicates the serialization logic the roadmap forbids, and is impossible
  against a root daemon. Do not do this.

Every new wire type/field follows the existing config discipline: additive,
`#[serde(default)]` where it rides on `LocalConfig`, covered by a back-compat
round-trip test (mirror `enabled_defaults_to_true_when_absent` and
`vpn_backend_defaults_to_network_manager_when_absent`).

## Failure modes (handle; test the pure parts)

- **Daemon not running / socket absent.** `NotRunning` → a clear, non-fatal "start
  the daemon" state; the GUI stays open and recovers on the next poll once it is up.
- **Permission denied (root daemon, unprivileged GUI).** `PermissionDenied` →
  show the client's existing guidance (run as the daemon's user/group). The GUI
  must **not** attempt sudo/escalation — that violates the zero-privilege mandate.
- **Protocol version skew.** GUI built for v2 vs a v1 daemon (or vice-versa) →
  explicit "version mismatch, update X" message, never a raw decode error.
- **Blocking/slow call.** A hung `send_request` must not freeze the window
  (worker thread); a single in-flight cap prevents request pile-up.
- **Mid-session daemon restart.** A poll failing then succeeding must reconcile
  the view without stale toggle/applied state.
- **Invalid edits.** Validate domains and config fields client-side before
  sending (pure, tested), and still render a daemon `Response::Error` (e.g.
  duplicate domain, persist failure) as a visible, dismissable message.

## Platform / build / CI

- `eframe` on Linux needs system GL/windowing dev libraries to **compile** (e.g.
  `libxkbcommon`, Wayland/X11, GL headers). Add the install step to the `check`
  job's Linux leg in `.github/workflows/ci.yml` so `cargo fmt`/`clippy`/`test`
  stay green on the ubuntu+macos matrix; macOS needs none.
- The same `ci.yml` has a `nix` job running `nix flake check` + `nix build`:
  eframe's native deps must also go into the flake derivation's
  `buildInputs`/`nativeBuildInputs` (and the devShell) or `nix build` fails to
  compile the GUI. Keep the flake green too (ties into Phase 0.5 packaging).
- The GUI is Unix-only like the CLI's IPC. Keep the workspace building on the
  Windows release target — exclude `splitway-gui` from the Windows build (or stub
  `main`) rather than pulling egui into a platform we don't ship; match how
  `splitway-cli` guards its Unix-only path. Document the choice.
- Do **not** add a windowed smoke test that needs a display in CI (would require
  `xvfb`); rely on pure-logic unit tests plus a compile check. No GUI packaging
  here — that is Phase 5.

## Out of scope

- Any `DnsBackend`/detector change, and any new privileged operation.
- Tray icon, desktop notifications, autostart, per-domain live status, theming,
  rule editing beyond the domain list — explicitly "nothing more in v1."
- GUI packaging/distribution (Phase 5); Windows GUI; proxy/route targets.
- Runtime config-file switching **if** deferred per the design decision above
  (record it as a named follow-up).

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macOS), with the Linux GL deps added.
- Against a running daemon on Linux **and** macOS: the toggle enables/disables
  (and the status block reflects `vpn_up`/`applied`), domains add/remove, and the
  config edits persist and reconcile — all via IPC, with the GUI holding no
  privileges and writing no files itself. Manual verification log in the PR.
- The IPC extension is additive and versioned: `PROTOCOL_VERSION` bumped, the new
  wire types round-trip, and version skew (the strict-equality reject) is surfaced
  as actionable "update" guidance, not a raw error. Daemon, CLI and GUI build from
  one workspace and upgrade in lockstep. `SetConfig` goes through the state actor's
  single-writer `commit()` path (no second config writer anywhere).
- Pure logic unit-tested: the view-model reducer, domain/field validation,
  `ClientError`→message mapping, and the new wire types' serde round-trip +
  back-compat. egui rendering and the worker thread remain thin, untested plumbing.
- `vpn_name`-needs-restart caveat surfaced in the UI, not hidden.
- `README.md` updated: a short "GUI" subsection (what it does, that it is a pure
  IPC client with no privileges, how it reaches a per-user vs system daemon) and
  the daemon `--config` flag if added.
- CLI and daemon behavior otherwise unchanged — no regression in existing tests.

## Finish

PR into `dev` titled `Phase 4: Primitive GUI`. Description: the
config-editing/selection-over-IPC design decision with rationale (and the
rejected direct-file-write alternative), the `PROTOCOL_VERSION` bump and
compatibility rule, the threading model, the privilege/reachability notes per
`ClientError` variant, screenshots of the GUI on Linux and macOS (**with any real domains/IPs redacted**), the manual
verification log, the done-criteria checklist, and any deferred follow-ups
(runtime config-file switching, etc.).
