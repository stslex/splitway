# Phase 7c — Mutations through the truth-contract

Let the Tauri GUI change config/routing. Every mutation is **daemon-first**: the
frontend sends a write command and never updates its own domain state; the change
reaches the screen only via the normal `view-model-changed` event. Also lands the
interactive **CheckDomain** one-shot query deferred from 7b.

Builds on 7b (the read path: poll thread → `ViewModelSnapshot` → event → render)
and assumes its no-authoritative-state invariant. egui stays a read/write
reference on its existing intent path, untouched.

## The truth contract — why it is correctness, not purity

The config is a **multi-writer SSOT** (architecture §1): the CLI, a hand-edit, or
another process can change it out-of-band — that is why the daemon has a file
watcher and a single-writer actor. So the state *after* your mutation may include
concurrent external changes you could not predict. Optimistic UI would render your
*predicted* result and be wrong; rendering the post-write view-model is correct for
free. That is the whole reason for the contract.

## Confirmed repo facts (no protocol change)

1. **Mutation surface = parity with the daemon's existing write verbs.** Exactly:
   `Enable`, `Disable`, `AddDomain`, `RemoveDomain`, `SetConfig` (the editable
   projection: `vpn_name` / `vpn_backend` / `openvpn.*` — the "set interface /
   backend" path; the CLI has no equivalent, the egui editor does), and
   `ReloadConfig` (resync). Modeled as the closed `splitway_gui_core::Mutation`
   enum so the surface is greppable and bounded. A verb beyond this is a separate
   decision **and** a protocol bump.
2. **CheckDomain and every write verb already exist at protocol v6.** So 7c is GUI
   + gui-core wiring only — **no protocol version change**.
3. **Refresh-now** is new wiring in the Tauri adapter (below); gui-core already
   decides *what* a refresh fetches (`GuiCore::poll`).

## The shape: the poll thread is the sole producer of view-models

7c keeps 7b's pipeline and bolts the write path onto its edges so the contract is
enforced **by construction**:

```
                          ┌─ get_view_model (mount) ─┐
 daemon ──poll thread (GuiCore)──► ViewModelSnapshot ─┼─ view-model-changed event ─► frontend render
   ▲          ▲  (the ONLY VM producer)              ─┘                                    │
   │          │ refresh-now wake (recv_timeout)                                            │ user action
   │          └──────────────────────────────────────────────────────────┐               ▼
   └──── mutation round-trip (blocking pool) ◄── run_mutation / run_check ◄┴──── Tauri command
```

- A mutation/check **command never touches the `GuiCore` or the shared VM.** It
  only round-trips the daemon (`splitway_gui_core::run_mutation` / `run_check`),
  on the async runtime's **blocking pool** so the webview's main thread never
  stalls. It returns a per-action `Result` (or a `CheckOutcome`) to the frontend.
- After a mutation, the command fires the **`RefreshSignal`** (refresh-now). The
  poll thread waits between cycles on that signal with a `POLL_INTERVAL` timeout
  (`recv_timeout`), so a mutation collapses the action→truth latency to ~one poll
  cycle instead of up to the full interval, while the scheduled timeout keeps the
  display live (and picks up out-of-band edits) when nothing is mutating.
- Because the poll thread is the **only** writer of the VM, no mutation can write
  displayed state. The contract is structural, not a convention: search the bridge
  for `SharedVm::set`/`emit` (poll thread only) and confirm the command path has no
  `GuiCore` in scope.

### Why mutations route *off* the poll thread (and so does check)

The alternative — a command channel into the poll thread that calls
`GuiCore::mutate` — would let `mutate` arm refresh-now *on the core* (a closer
match to "gui-core owns the signal"), but it makes a mutation block the live
status poll for everyone, and a slow `CheckDomain` resolution would stall the
whole display. Keeping the command path stateless and off the poll thread isolates
that latency. "gui-core owns refresh-now" is honored in spirit: gui-core's
`poll()` defines *what* the refresh fetches; the adapter's wake decides *when* —
the same "core decides what, driver decides when" split the 7b read path uses.

### Refresh-now fires on every outcome, not only `Ok`

A rejected write may still have reconciled daemon state (e.g. a duplicate-add that
adopts a concurrent external edit before returning the error), and a transport
failure must move the VM to the disconnected variant. So the command fires
refresh-now regardless; `should_emit` dedups a genuine no-op, so the extra poll is
harmless.

## Request-lifecycle state is a distinct, allowed category

The no-optimistic-UI rule bans predicting the daemon's *resulting config*. It does
**not** ban ephemeral facts about the in-flight interaction. The frontend holds one
small, clearly-separated store (`ui/src/lifecycle.ts`):

- **pending** per action (drives the disabled + "…" indicators, double-submit guard),
- **last error** per action (a daemon `Response::Error` or a transport failure),
- the **CheckDomain result** (ephemeral; never folded into the VM),
- the **config-editor input buffers** + a **dirty** latch.

Domain/config/status truth is rendered **solely** from the cached `lastVm`
(assigned only on the event/initial-fetch path). The grep-invariant: writes to
`lastVm` live only in `applyVm`; everything else is the lifecycle store.

**Pending clears on command resolution (Ok or Err), not by content-correlating the
next VM.** Full-snapshot events carry no per-mutation correlation, so matching "did
this snapshot incorporate my write?" is fragile under idempotent/concurrent writes.
We clear on resolution and rely on refresh-now to make the VM catch up.

**Config editor dirty-guard.** The form is pre-filled from `vm.config` only while
clean and no save is in flight, so a background poll cannot clobber an in-progress
edit (the same guard gui-core's egui editor uses). A successful save clears dirty,
so the next VM event re-adopts the daemon-normalized values — which also resolves
the egui `TODO(7c)` "live buffer vs sent value" drift at the source for this path.

## Validation: the daemon is the authoritative validator

The frontend does only light **input hygiene** — an empty add/check field disables
its submit button. Everything authoritative (valid domain? duplicate? OpenVPN
needs a management endpoint? socket-group field lock?) is daemon-side and surfaces
as a command `Err`, shown as the per-action error. Nothing is pre-accepted locally.

## Frozen-on-malformed: reject, already enforced by the daemon

(Open decision #2 → **reject-with-explanation**, and it needed no daemon change.)
Architecture §1: a malformed on-disk config blocks every IPC mutation, because a
write derived from a config the daemon could not read would clobber the fields it
preserves from disk. The daemon already returns a clear
`config_unreadable_reply` ("cannot change settings: the config file on disk could
not be read … fix it on disk; the daemon keeps running on the last-good config").
7c surfaces it two ways: the mutation's `Err` becomes the per-action error, and the
frozen state — already the `RoutingState::ConfigInvalid` VM variant from 5c — is
shown as a prominent banner. A corrupt config is repaired *on disk*, never through
the GUI.

## CheckDomain — a one-shot query, never VM state

`check_domain(host)` is a Tauri command returning its own `CheckOutcome`
(`Checked { result }` | `Error { message }`), rendered in an ephemeral area. It is
**not** folded into the polled VM (the boundary recorded in
[`tauri-read-only.md`](tauri-read-only.md)): a parameterized query result is not
ambient config truth, and folding it in would mean N live resolutions per poll
cycle. It never fires refresh-now and runs on the blocking pool so a slow resolver
never stalls the poll thread.

## Type-sharing

`CheckOutcome` + `DomainCheckInfo` + `ResolutionInfo` are hand-mirrored in
`ui/src/bindings/view-model.ts`, like the VM. The gui-core
`check_outcome_serializes_internally_tagged_on_state` test locks the Rust→JSON
shape; `contract-check.ts` keeps the discriminated union exhaustively handled. The
**view-model itself is unchanged** in 7c (pending/error/check are frontend
lifecycle state, not VM fields), so the 7b serialize-vs-fixture guard is untouched.

## Scope / out of scope

- **In:** the gui-core command path (`Mutation`, `run_mutation`, `CheckOutcome`,
  `run_check`); the Tauri `RefreshSignal` + `recv_timeout` poll loop + the six
  mutation/check commands; the frontend mutation controls + lifecycle store +
  CheckDomain UI; the frozen banner.
- **Out (7d):** visual design / theming, window behavior, niri window rules,
  bundling. **Out (separate decision + protocol bump):** any verb the CLI/daemon
  does not already expose. **Out:** retiring the egui reference (it stays, untouched).

## Verification

- **Unit (gui-core, fake daemon):** `run_mutation` issues the right verb and maps
  `Ok`/daemon-`Error`/transport/unexpected; `run_check` maps every reply; both are
  stateless (cannot touch the VM). `CheckOutcome` serde shape.
- **Unit (bridge, fake daemon):** `run_mutation_and_refresh` returns the per-action
  result and fires refresh-now on Ok / daemon-error / transport; the **central
  truth-contract test** — a mutation never changes the snapshot, only the
  refresh-now re-poll does. `ConfigInput` drops `config_path`.
- **Frontend:** `tsc --noEmit` (incl. the contract guards) + esbuild bundle.
- **Live (niri, unprivileged):** the GUI renders the full mutation UI (toggle,
  add/remove, config editor, check, resync) with no webkit errors; an out-of-band
  CLI add appears via the VM-event path (the concurrency proof: merged external
  truth displayed correctly); corrupting the config shows the frozen banner and a
  mutation is rejected with the "fix it on disk" message; CheckDomain returns its
  result ephemerally. (Clicking the GUI's own buttons is the manual step — no
  Wayland input-injection tool was available — but the command path it drives is
  `run_mutation_and_refresh`, unit-tested + proven against the live daemon.)

## Links

- [Architecture §1 — config is the SSOT, read fresh](../architecture.md): the
  multi-writer SSOT + freeze-on-malformed this builds on.
- [Architecture §2 — GUI mutation truth contract](../architecture.md): the invariant
  7c enforces structurally for the Tauri frontend.
- [`tauri-read-only.md`](tauri-read-only.md): the 7b read path + the
  ambient-vs-parameterized boundary CheckDomain honors.
- [`gui-core-extraction.md`](gui-core-extraction.md): the framework-agnostic core
  the command path extends.
