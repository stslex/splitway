//! The Tauri ↔ `GuiCore` bridge: the testable pieces of the read **and** write
//! paths.
//!
//! Read path (7b) — these unit-test without spinning up a webview:
//!
//! - [`SharedVm`] — the current [`ViewModelSnapshot`] behind a mutex, shared
//!   between the poll thread (writer) and the [`get_view_model`] command (reader).
//! - [`step`] — drive the `GuiCore` through one whole poll cycle against an
//!   injected request sender, returning the resulting snapshot. The real poll
//!   thread injects the blocking socket client; tests inject a fake daemon.
//! - [`should_emit`] — the emit-on-change decision (full-VM, last-wins; never
//!   deltas), so identical snapshots do not spam the frontend with events.
//!
//! Write path (7c) — mutations are daemon-first with **no optimistic UI**, and
//! the truth-contract invariant is enforced *by construction*: the poll thread is
//! the **only** producer of view-models, and a mutation/query command has no
//! access to the `GuiCore` or the [`SharedVm`] at all (it only round-trips the
//! daemon). So a mutation can never write displayed state — that changes solely
//! when the poll thread re-polls and emits.
//!
//! - [`RefreshSignal`] — the **refresh-now** trigger. A successful (or failed)
//!   mutation fires it; the poll thread waits on it with a timeout, so the action
//!   → truth latency collapses to ~one poll cycle instead of up to [`POLL_INTERVAL`].
//! - [`run_mutation_and_refresh`] — run one mutating round-trip via
//!   `splitway_gui_core::run_mutation`, then fire refresh-now. Returns the
//!   per-action `Result` for the frontend's request-lifecycle store. Unit-tested
//!   with a fake daemon + a fake refresh sink.
//! - The `#[tauri::command]` wrappers ([`set_enabled`], [`add_domain`],
//!   [`remove_domain`], [`set_config`], [`reload`], [`check_domain`]) run the
//!   blocking round-trip on the async runtime's blocking pool so the webview's
//!   main thread never stalls. [`check_domain`] is the one-shot route-check; it
//!   does **not** refresh (a query result is not VM truth).
//!
//! [`poll_loop`] composes the read-path helpers with the actual socket client,
//! the `AppHandle` event push, and the refresh-now wait — that wiring is thin
//! and, like the egui worker, is not unit-tested (it has no decision logic).

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, State};

use splitway_shared::config::VpnBackend;
use splitway_shared::ipc::client::{self, ClientError};
use splitway_shared::ipc::{ConfigView, Request, Response};

use splitway_gui_core::{
    run_check, run_mutation, CheckOutcome, GuiCore, Mutation, ViewModelSnapshot,
};

/// The event name carrying a full view-model snapshot to the frontend. Lowercase
/// with a dash, per Tauri's event-name charset (`[a-z0-9-/:_]`); the frontend
/// `listen`s for exactly this.
pub const VIEW_MODEL_CHANGED: &str = "view-model-changed";

/// How often the poll thread re-polls the daemon, matching the egui harness — the
/// protocol has no server push, so a client polls to stay live.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// The current view-model, shared between the poll thread and the
/// [`get_view_model`] command. A cheap-to-clone `Arc<Mutex<…>>` so the thread can
/// own a handle that outlives Tauri's `setup`.
#[derive(Clone)]
pub struct SharedVm(Arc<Mutex<ViewModelSnapshot>>);

impl Default for SharedVm {
    /// Seed with a fresh core's snapshot — the "connecting…" state (health
    /// `Unknown`, nothing loaded) — so a frontend that mounts before the first
    /// poll completes still gets a coherent VM rather than an empty/None.
    fn default() -> Self {
        SharedVm(Arc::new(Mutex::new(GuiCore::new().snapshot())))
    }
}

impl SharedVm {
    /// Read the current snapshot (a clone, so the lock is held only briefly).
    ///
    /// Poison-tolerant: the lock only ever guards a single assignment/clone, so a
    /// poisoned mutex (a holder panicked) carries no torn state — recover the
    /// inner value rather than propagating the panic and freezing the poll thread.
    pub fn current(&self) -> ViewModelSnapshot {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the current snapshot. Called by the poll thread each cycle.
    /// Poison-tolerant for the same reason as [`SharedVm::current`].
    pub fn set(&self, snapshot: ViewModelSnapshot) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }
}

/// The single read command: return the whole current view-model. Used once by the
/// frontend on mount; all subsequent updates arrive as [`VIEW_MODEL_CHANGED`]
/// events. No per-field commands, no frontend polling.
#[tauri::command]
pub fn get_view_model(state: State<'_, SharedVm>) -> ViewModelSnapshot {
    state.current()
}

// --- write path: refresh-now + mutation/check commands ------------------

/// The **refresh-now** trigger: a wake the poll thread waits on (with a timeout)
/// so a mutation's effect appears within ~one poll cycle instead of up to
/// [`POLL_INTERVAL`]. gui-core decides *what* a refresh fetches (`GuiCore::poll`);
/// this decides *when* — the same "core decides what, driver decides when" split
/// the read path uses, here driven by a mutation rather than the timer.
///
/// A cheap-to-clone handle (Tauri-managed; cloned into each command). The `Mutex`
/// makes it `Sync` for Tauri state and serializes the (trivial) sends. `fire` is
/// best-effort: a closed channel (poll thread gone / app shutting down) is ignored.
#[derive(Clone)]
pub struct RefreshSignal {
    tx: Arc<Mutex<Sender<()>>>,
}

impl RefreshSignal {
    /// Build a signal over `tx`; the poll thread holds the matching `Receiver`.
    pub fn new(tx: Sender<()>) -> Self {
        RefreshSignal {
            tx: Arc::new(Mutex::new(tx)),
        }
    }

    /// Request an immediate poll cycle. Best-effort and never blocks meaningfully
    /// (the channel is unbounded); a send error means the poll thread is gone.
    pub fn fire(&self) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(());
    }
}

/// The editable config a `set_config` command carries from the frontend — the
/// wire-facing half of [`ConfigView`] minus the daemon-owned `config_path` (fixed
/// at launch, ignored on write). Deserialized from the single `view` command arg.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConfigInput {
    pub vpn_name: String,
    pub vpn_backend: VpnBackend,
    pub openvpn_management: String,
    pub openvpn_management_password_file: Option<String>,
}

impl From<ConfigInput> for ConfigView {
    fn from(input: ConfigInput) -> ConfigView {
        ConfigView {
            vpn_name: input.vpn_name,
            vpn_backend: input.vpn_backend,
            openvpn_management: input.openvpn_management,
            openvpn_management_password_file: input.openvpn_management_password_file,
            // The daemon ignores this on SetConfig (the active path is fixed at
            // launch); send empty rather than echoing a value the GUI must not set.
            config_path: String::new(),
        }
    }
}

/// Run one mutating round-trip and fire refresh-now, returning the per-action
/// `Result` for the frontend's lifecycle store. The truth contract holds because
/// this touches **no** view-model: it round-trips the daemon, then asks the poll
/// thread to re-poll — the VM (not this command) carries the new truth.
///
/// Refresh-now fires on *every* outcome, not only `Ok`: a rejected write may
/// still have reconciled daemon state (e.g. a duplicate-add that adopts a
/// concurrent external edit), and a transport failure must move the VM to the
/// disconnected variant. [`should_emit`] dedups a genuine no-op, so an extra poll
/// is harmless.
pub fn run_mutation_and_refresh<F>(
    mutation: Mutation,
    send: F,
    refresh: &RefreshSignal,
) -> Result<(), String>
where
    F: FnOnce(Request) -> Result<Response, ClientError>,
{
    let result = run_mutation(mutation, send);
    refresh.fire();
    result
}

/// Map `enabled` to the enable/disable verb and run it daemon-first.
#[tauri::command]
pub async fn set_enabled(enabled: bool, refresh: State<'_, RefreshSignal>) -> Result<(), String> {
    let mutation = if enabled {
        Mutation::Enable
    } else {
        Mutation::Disable
    };
    dispatch_mutation(mutation, refresh.inner().clone()).await
}

/// Add a routing domain (the daemon normalizes + validates the raw input).
#[tauri::command]
pub async fn add_domain(domain: String, refresh: State<'_, RefreshSignal>) -> Result<(), String> {
    dispatch_mutation(Mutation::AddDomain(domain), refresh.inner().clone()).await
}

/// Remove a routing domain.
#[tauri::command]
pub async fn remove_domain(
    domain: String,
    refresh: State<'_, RefreshSignal>,
) -> Result<(), String> {
    dispatch_mutation(Mutation::RemoveDomain(domain), refresh.inner().clone()).await
}

/// Update the editable config projection (interface / backend / OpenVPN).
#[tauri::command]
pub async fn set_config(
    view: ConfigInput,
    refresh: State<'_, RefreshSignal>,
) -> Result<(), String> {
    dispatch_mutation(Mutation::SetConfig(view.into()), refresh.inner().clone()).await
}

/// Resync: ask the daemon to re-read its config from disk and reconcile.
#[tauri::command]
pub async fn reload(refresh: State<'_, RefreshSignal>) -> Result<(), String> {
    dispatch_mutation(Mutation::Reload, refresh.inner().clone()).await
}

/// Run a mutation on the async runtime's **blocking** pool (the IPC client is a
/// blocking socket round-trip), so the webview's main thread never stalls.
async fn dispatch_mutation(mutation: Mutation, refresh: RefreshSignal) -> Result<(), String> {
    match tauri::async_runtime::spawn_blocking(move || {
        run_mutation_and_refresh(mutation, client::send_request, &refresh)
    })
    .await
    {
        Ok(result) => result,
        // The blocking task panicked or was cancelled — surface it rather than a
        // silent hang; the frontend clears its pending state on this resolution.
        Err(e) => Err(format!("internal error: mutation task failed: {e}")),
    }
}

/// The one-shot route-check ([`Request::CheckDomain`]). Returns its own
/// [`CheckOutcome`] for an **ephemeral** result area — it is never folded into the
/// polled view-model and never triggers refresh-now (a parameterized query result
/// is not ambient config truth). Runs on the blocking pool because a live
/// resolution can be slow; keeping it off the poll thread means a slow resolver
/// never stalls the live status display.
#[tauri::command]
pub async fn check_domain(domain: String) -> CheckOutcome {
    match tauri::async_runtime::spawn_blocking(move || run_check(domain, client::send_request))
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => CheckOutcome::Error {
            message: format!("internal error: check task failed: {e}"),
        },
    }
}

/// Drive `core` through one whole poll cycle and return the resulting snapshot.
///
/// `send` performs a single request→reply round-trip (the real thread uses the
/// blocking socket client; tests inject a fake). The cycle: if the core is idle,
/// enqueue a fresh poll; then drain *every* queued request (including the
/// reconnect-edge refetch the first `Status` may enqueue) before snapshotting —
/// so `Status` (belief) and `Verify` (reality) in the returned snapshot are from
/// the same cycle, and the snapshot is never a partial assembly.
pub fn step<F>(core: &mut GuiCore, send: F) -> ViewModelSnapshot
where
    F: Fn(Request) -> Result<Response, ClientError>,
{
    // Only poll when idle: a fresh core is primed with a `Status` (not idle), so
    // the first cycle drains that + its reconnect-edge refetch without
    // double-queuing; later cycles, arriving idle, enqueue the periodic poll.
    if core.is_idle() {
        core.poll();
    }
    while let Some(request) = core.take_next_request() {
        let result = send(request.clone());
        core.apply_reply(request, result);
    }
    core.snapshot()
}

/// Whether a freshly built snapshot differs from the last one emitted — the
/// emit-on-change gate. `None` (nothing emitted yet) always emits.
pub fn should_emit(last_emitted: &Option<ViewModelSnapshot>, current: &ViewModelSnapshot) -> bool {
    last_emitted.as_ref() != Some(current)
}

/// The poll thread: drive the core forever, publishing each cycle's snapshot to
/// `shared` and emitting a full-VM event whenever it changed. Composes [`step`],
/// [`SharedVm::set`], and [`should_emit`] with the real socket client and the
/// `AppHandle` event push. Thin plumbing — the decisions are in the helpers above.
///
/// This thread is the **sole** producer of view-models (the truth-contract
/// anchor): every mutation routes its effect back here via a re-poll, never by
/// touching the VM directly. Between cycles it waits on `refresh_rx` for up to
/// [`POLL_INTERVAL`] — a mutation's refresh-now wake collapses that wait so the
/// new truth appears promptly, while the scheduled timeout keeps the display live
/// (and picks up out-of-band edits) when nothing is mutating.
pub fn poll_loop(app: AppHandle, shared: SharedVm, refresh_rx: Receiver<()>) {
    let mut core = GuiCore::new();
    let mut last_emitted: Option<ViewModelSnapshot> = None;
    loop {
        // A panic in a cycle must not silently kill the poll thread and freeze the
        // UI (no further events would ever arrive). Catch it, reset the core (which
        // re-primes a fresh Status) so a wedged in-flight slot cannot strand the
        // loop, and carry on. `AssertUnwindSafe` is sound here: on a panic we
        // discard the possibly-inconsistent `core`/`last_emitted` and rebuild.
        let cycle = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let snapshot = step(&mut core, client::send_request);
            // Publish for `get_view_model` (a frontend mounting between events).
            shared.set(snapshot.clone());
            if should_emit(&last_emitted, &snapshot) {
                // Only advance `last_emitted` on a *successful* emit, so a
                // transient failure (e.g. the webview briefly unavailable) is
                // retried next cycle instead of being silently dropped when the
                // snapshot is unchanged.
                match app.emit(VIEW_MODEL_CHANGED, snapshot.clone()) {
                    Ok(()) => last_emitted = Some(snapshot),
                    Err(err) => log::warn!("failed to emit {VIEW_MODEL_CHANGED}: {err}"),
                }
            }
        }));
        if cycle.is_err() {
            log::error!("splitway poll cycle panicked; resetting the core and continuing");
            core = GuiCore::new();
            last_emitted = None;
        }
        // Wait for a refresh-now wake or the poll interval, whichever comes first.
        match refresh_rx.recv_timeout(POLL_INTERVAL) {
            // A mutation asked for an immediate re-poll: coalesce a burst (several
            // quick mutations) into this one cycle, then loop straight into it.
            Ok(()) => while refresh_rx.try_recv().is_ok() {},
            // No wake within the interval: the ordinary scheduled poll.
            Err(RecvTimeoutError::Timeout) => {}
            // Every `RefreshSignal` sender dropped — the app is shutting down.
            Err(RecvTimeoutError::Disconnected) => {
                log::debug!("refresh channel closed; stopping the splitway poll thread");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use splitway_gui_core::model::Health;
    use splitway_gui_core::VerifyView;
    use splitway_shared::config::VpnBackend;
    use splitway_shared::ipc::{
        AppliedInfo, ConfigView, DetectorHealth, DriftVerdict, InterfaceInfo, LinkDnsState,
        RoutingState, StatusInfo, VerifyInfo,
    };

    /// A fake connected daemon: canned replies for every verb the read-only poll
    /// cycle issues. Belief (Status.applied) and reality (Verify.live) match, so
    /// drift is `InSync`.
    fn connected_daemon(request: Request) -> Result<Response, ClientError> {
        match request {
            Request::Status => Ok(Response::Status(StatusInfo {
                enabled: true,
                interface: "tun0".to_string(),
                vpn_up: true,
                applied: Some(AppliedInfo {
                    interface: "tun0".to_string(),
                    domains: vec!["corp.example.com".to_string()],
                    dns_servers: vec!["10.0.0.1".to_string()],
                }),
                routing_state: RoutingState::Applied,
                detected_dns: vec!["10.0.0.1".to_string()],
                detector_health: DetectorHealth::Active,
                domains: vec!["corp.example.com".to_string()],
            })),
            Request::GetConfig => Ok(Response::Config(ConfigView {
                vpn_name: "tun0".to_string(),
                vpn_backend: VpnBackend::NetworkManager,
                openvpn_management: String::new(),
                openvpn_management_password_file: None,
                config_path: "/etc/splitway/config.json".to_string(),
            })),
            Request::ListInterfaces => Ok(Response::Interfaces(vec![InterfaceInfo {
                name: "tun0".to_string(),
                up: true,
                vpn_like: true,
            }])),
            Request::Verify => Ok(Response::Verify(VerifyInfo {
                live: LinkDnsState {
                    servers: vec!["10.0.0.1".to_string()],
                    routing_domains: vec!["corp.example.com".to_string()],
                },
                drift: DriftVerdict::InSync,
            })),
            // No mutating verbs are issued on the read-only path.
            other => Ok(Response::Error(format!("unexpected request: {other:?}"))),
        }
    }

    /// A daemon that is down: every connect fails.
    fn down_daemon(_request: Request) -> Result<Response, ClientError> {
        Err(ClientError::NotRunning(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no socket",
        )))
    }

    #[test]
    fn step_first_cycle_connects_and_loads_config_without_verify_yet() {
        let mut core = GuiCore::new();
        // First cycle: drains the primed Status + the reconnect-edge refetch
        // (GetConfig + ListInterfaces). Verify is only added by poll(), which the
        // first (non-idle) cycle does not call — so verify is still Unknown.
        let snap = step(&mut core, connected_daemon);
        assert_eq!(snap.connection.health, Health::Connected);
        assert!(snap.connected);
        assert!(snap.config_loaded);
        assert_eq!(snap.config.as_ref().unwrap().vpn_name, "tun0");
        assert_eq!(snap.interfaces.len(), 1);
        assert_eq!(snap.verify, VerifyView::Unknown);
    }

    #[test]
    fn step_second_cycle_includes_verify_with_coherent_drift() {
        let mut core = GuiCore::new();
        let _ = step(&mut core, connected_daemon);
        // Second cycle is idle → poll() adds Verify; belief == reality → InSync.
        let snap = step(&mut core, connected_daemon);
        match snap.verify {
            VerifyView::Available { drift, .. } => assert_eq!(drift, DriftVerdict::InSync),
            other => panic!("expected Available verify, got {other:?}"),
        }
    }

    #[test]
    fn step_against_a_down_daemon_yields_a_disconnected_snapshot() {
        let mut core = GuiCore::new();
        let snap = step(&mut core, down_daemon);
        assert_eq!(snap.connection.health, Health::NotRunning);
        assert!(!snap.connected);
        // The banner message carries the client's actionable guidance verbatim.
        assert!(snap.connection.message.is_some());
        // No live status is shown when disconnected.
        assert!(snap.status.is_none());
    }

    #[test]
    fn shared_vm_round_trips_through_the_command_path() {
        let shared = SharedVm::default();
        // The seed is a fresh core's snapshot — connecting, nothing loaded.
        assert_eq!(shared.current().connection.health, Health::Unknown);

        // Driving a cycle and publishing makes the command return the new VM.
        let mut core = GuiCore::new();
        let snap = step(&mut core, connected_daemon);
        shared.set(snap.clone());
        assert_eq!(shared.current(), snap);
        assert_eq!(shared.current().connection.health, Health::Connected);
    }

    #[test]
    fn should_emit_only_on_change() {
        let mut core = GuiCore::new();
        let first = step(&mut core, connected_daemon);
        // Nothing emitted yet → emit.
        assert!(should_emit(&None, &first));
        // Same snapshot again → do not emit.
        assert!(!should_emit(&Some(first.clone()), &first));

        // A cycle that changes the VM (verify now Available) → emit.
        let second = step(&mut core, connected_daemon);
        assert_ne!(first, second);
        assert!(should_emit(&Some(first), &second));
    }

    /// The bindings type-drift guard (the Rust half; the TS half is the
    /// compile-time check in `ui/src/contract-check.ts`). A representative
    /// connected snapshot must
    /// serialize to exactly the committed `ui/src/bindings/view-model.sample.json`
    /// — so any change to the Rust view-model shape (a renamed/added/removed
    /// field, a changed enum repr) fails this test and forces both the fixture
    /// and the hand-written TS mirror to be updated in lockstep.
    ///
    /// Regenerate after an intended shape change:
    ///   UPDATE_VIEW_MODEL_FIXTURE=1 cargo test -p splitway-gui-tauri --lib fixture
    /// then update `ui/src/bindings/view-model.ts` to match.
    #[test]
    fn bindings_fixture_matches_the_view_model_shape() {
        // A representative VM: two connected cycles → status applied, config
        // loaded, interfaces enumerated, verify Available + InSync.
        let mut core = GuiCore::new();
        let _ = step(&mut core, connected_daemon);
        let sample = step(&mut core, connected_daemon);
        let actual = serde_json::to_value(&sample).expect("serialize sample VM");

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/src/bindings/view-model.sample.json"
        );
        if std::env::var_os("UPDATE_VIEW_MODEL_FIXTURE").is_some() {
            let pretty = serde_json::to_string_pretty(&actual).expect("pretty-print");
            std::fs::write(path, format!("{pretty}\n")).expect("write fixture");
            return;
        }

        let committed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(path)
                .expect("fixture missing — run with UPDATE_VIEW_MODEL_FIXTURE=1"),
        )
        .expect("parse committed fixture");
        assert_eq!(
            actual, committed,
            "view-model shape changed — regenerate the fixture (UPDATE_VIEW_MODEL_FIXTURE=1 \
             cargo test -p splitway-gui-tauri --lib fixture) and update \
             ui/src/bindings/view-model.ts to match"
        );
    }

    // --- write path: mutation + refresh-now (7c) --------------------------

    #[test]
    fn run_mutation_and_refresh_returns_ok_issues_the_verb_and_fires_refresh() {
        let (tx, rx) = mpsc::channel();
        let refresh = RefreshSignal::new(tx);
        let seen = std::cell::Cell::new(None);
        let result = run_mutation_and_refresh(
            Mutation::AddDomain("corp.example.com".to_string()),
            |request| {
                seen.set(Some(request));
                Ok(Response::Ok)
            },
            &refresh,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            seen.into_inner(),
            Some(Request::AddDomain("corp.example.com".to_string()))
        );
        assert!(
            rx.try_recv().is_ok(),
            "a successful mutation must fire refresh-now"
        );
    }

    #[test]
    fn run_mutation_and_refresh_surfaces_a_daemon_error_and_still_refreshes() {
        let (tx, rx) = mpsc::channel();
        let refresh = RefreshSignal::new(tx);
        let result = run_mutation_and_refresh(
            Mutation::AddDomain("dup.example.com".to_string()),
            |_| {
                Ok(Response::Error(
                    "domain already present: dup.example.com".to_string(),
                ))
            },
            &refresh,
        );
        assert_eq!(
            result,
            Err("domain already present: dup.example.com".to_string())
        );
        // Still fires: a rejected write may have reconciled daemon state, and the
        // VM (not the command) is the source of truth.
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn run_mutation_and_refresh_fires_on_transport_failure_for_the_disconnected_variant() {
        let (tx, rx) = mpsc::channel();
        let refresh = RefreshSignal::new(tx);
        let result = run_mutation_and_refresh(Mutation::Disable, down_daemon, &refresh);
        assert!(result.is_err());
        assert!(
            rx.try_recv().is_ok(),
            "refresh-now must fire so the VM moves to the disconnected variant"
        );
    }

    #[test]
    fn config_input_drops_the_daemon_owned_config_path() {
        let view: ConfigView = ConfigInput {
            vpn_name: "tun0".to_string(),
            vpn_backend: VpnBackend::NetworkManager,
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
        }
        .into();
        assert_eq!(view.vpn_name, "tun0");
        // The active path is fixed at daemon launch; the GUI must not set it.
        assert!(view.config_path.is_empty());
    }

    /// The central truth-contract check: a mutation never changes the displayed
    /// state itself — only the refresh-now re-poll does. Proven with a stateful
    /// fake daemon whose `Status` reflects a committed add: the mutation returns
    /// `Ok` and fires refresh, but the poll thread's snapshot is unchanged until
    /// the next `step` (the re-poll) surfaces the daemon's new truth.
    #[test]
    fn the_vm_changes_only_via_the_re_poll_after_a_mutation_never_the_command() {
        use std::cell::Cell;
        let added = Cell::new(false);
        let daemon = |request: Request| -> Result<Response, ClientError> {
            match request {
                Request::AddDomain(_) => {
                    added.set(true);
                    Ok(Response::Ok)
                }
                Request::Status => Ok(Response::Status(StatusInfo {
                    enabled: true,
                    interface: "tun0".to_string(),
                    vpn_up: true,
                    applied: None,
                    routing_state: RoutingState::VpnDown,
                    detected_dns: vec![],
                    detector_health: DetectorHealth::Active,
                    domains: if added.get() {
                        vec!["corp.example.com".to_string()]
                    } else {
                        vec![]
                    },
                })),
                Request::GetConfig => Ok(Response::Config(ConfigView {
                    vpn_name: "tun0".to_string(),
                    vpn_backend: VpnBackend::NetworkManager,
                    openvpn_management: String::new(),
                    openvpn_management_password_file: None,
                    config_path: "/etc/splitway/config.json".to_string(),
                })),
                Request::ListInterfaces => Ok(Response::Interfaces(vec![])),
                Request::Verify => Ok(Response::Verify(VerifyInfo {
                    live: LinkDnsState::default(),
                    drift: DriftVerdict::NotApplicable,
                })),
                other => Ok(Response::Error(format!("unexpected: {other:?}"))),
            }
        };

        let mut core = GuiCore::new();
        // Connect: the snapshot starts with no domains.
        let snap = step(&mut core, daemon);
        assert!(snap.status.as_ref().unwrap().domains.is_empty());

        // Run the mutation exactly as the command would — off the core, via the
        // daemon round-trip only. It returns Ok and fires refresh, but it has no
        // access to the core/VM, so the snapshot cannot change here.
        let (tx, rx) = mpsc::channel();
        let refresh = RefreshSignal::new(tx);
        let result = run_mutation_and_refresh(
            Mutation::AddDomain("corp.example.com".to_string()),
            daemon,
            &refresh,
        );
        assert_eq!(result, Ok(()));
        assert!(rx.try_recv().is_ok());
        assert!(
            core.snapshot().status.as_ref().unwrap().domains.is_empty(),
            "the mutation must not change the displayed state"
        );

        // Only the refresh-now re-poll surfaces the daemon's new truth.
        let snap = step(&mut core, daemon);
        assert_eq!(
            snap.status.as_ref().unwrap().domains,
            vec!["corp.example.com".to_string()],
            "the new domain must arrive via the re-poll, not the command"
        );
    }
}
