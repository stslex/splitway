# Framework-agnostic `splitway-gui-core`

The decision behind Phase 7a: extract the reusable GUI logic — the pure view-model
**and** the truth-contract orchestration — out of the egui crate into a new
`splitway-gui-core` crate that depends on `splitway-shared` only. This is a
prerequisite for the Phase 7 Tauri GUI: the Tauri backend drives the same core, so
the [GUI mutation truth contract](../architecture.md) §2 is implemented **once** and
both frontends cannot drift apart. It is a **pure refactor** — the egui harness
behaves identically.

## The agreement

- **Extract the orchestration, not just `model.rs`.** The stateless view-model
  helpers (`model.rs`) were already egui-free; the genuine decision was to also pull
  out the *stateful* orchestration that lived interleaved in `app.rs` — connection
  state, the reconnect-edge refetch policy, reply folding, `finish_action`,
  unsaved-edit preservation, the request queue, and the view-model — into a `GuiCore`
  state machine. The lighter alternative (move only `model.rs`, let Tauri reimplement
  the orchestration) was **rejected**: it would duplicate the truth contract across
  two frontends and invite divergence — exactly what §2 says to avoid.
- **`GuiCore` owns the state + the request queue; the frontend owns rendering and
  the socket.** The boundary:
  - The core holds the connection state, live status, interfaces, the editable
    config buffers, the last-synced snapshot, the transient message, and the
    outbound `Request` queue with at-most-one-in-flight (`inflight`).
  - It exposes `apply_reply(request, result)` (the only place displayed state
    changes), `view() -> ViewModel<'_>` (a borrowed, zero-copy read projection),
    `editor_mut()` (the editable buffers a frontend binds inputs to), the dispatch
    surface (`is_idle` / `poll` / `take_next_request`), and intent methods
    (`enable` / `disable` / `add_domain` / `remove_domain` / `save_config` /
    `reload_config` / `dismiss_message`).
  - The frontend (egui today, Tauri in 7b) owns *only* rendering, the worker channel
    plumbing, the poll **timing** (the core decides *what* to poll, the frontend
    *when*), and transient UI-local input (the unsubmitted domain text field, the
    file-picker launch hint) — neither of which is part of the truth contract.
- **No optimistic UI is structural, not incidental.** Intent methods only enqueue a
  request (and `add_domain` / `save_config` validate first); they never mutate the
  displayed state. A save is not marked "synced" (the editor's clean baseline is not
  advanced) until its `SetConfig` reply confirms `Ok`. Displayed state changes solely
  in `apply_reply`, from the daemon's reply.
- **The editable buffers live in the core, lent mutably for binding.** egui's
  immediate mode binds widgets to `&mut String` / `&mut VpnBackend`, so the core
  lends them via `editor_mut()`. The core needs to own them anyway: unsaved-edit
  detection compares the live buffers against the last-synced `ConfigSnapshot`, and
  that comparison is what stops a reconnect/poll refresh from clobbering an
  in-progress edit.

## Scope / out of scope

- **In:** the new crate; `model.rs` + its tests moved in verbatim; `GuiCore` with the
  orchestration extracted from `app.rs`; egui refactored to a thin
  rendering+plumbing driver of the core; new egui-free `GuiCore` unit tests.
- **Out:** Tauri (7b), any new protocol verb, visual design, bundling. The
  `CheckDomain` / `Verify` view-model is **not** added speculatively — it lands when
  7b/7c render it. Those verbs stay folded as no-ops in `apply_reply`, exactly as the
  egui app already ignored them.

## Notable choices / tradeoffs

- **`view()` returns a borrowed projection (`ViewModel<'a>`), not an owned snapshot.**
  Zero-copy per frame; the frontend copies out the few primitives it needs before
  calling a mutable core intent (the same clone-for-render pattern the egui code
  already used for the domain list and the message). The editable buffers are
  deliberately *not* in `ViewModel` — they are mutable and reached via `editor_mut()`.
- **`inflight` moves from the frontend's `pump` to the core's `take_next_request`.**
  The semantics are preserved (at most one request outstanding, set when dispatched,
  cleared when its reply folds). The only difference is shutdown-only: if the worker
  channel send fails because the window is closing, the request is dropped and
  `inflight` stays set — irrelevant because the app is exiting.
- **Unix-only, `cfg(unix)`-gated like the rest.** The shared IPC client
  (`splitway_shared::ipc::client`, hence `ClientError`) is `#[cfg(unix)]`, so the
  core's modules are gated the same way. A whole-workspace build on a non-Unix target
  still compiles `splitway-gui-core` (as an empty crate), matching how `splitway-gui`
  guards its egui stack.

## Links

- [Architecture §2 — GUI mutation truth contract](../architecture.md): the invariant
  `GuiCore` now implements once for all frontends.
- [Architecture §4 — one package, one version](../architecture.md): why the
  version-mismatch peek is the only protocol-skew the core handles.
- [`ROADMAP.md`](../../ROADMAP.md): Phase 7 decomposition (7a core → 7b Tauri shell →
  7c mutations → 7d visual design).
