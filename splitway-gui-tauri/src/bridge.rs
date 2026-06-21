//! The Tauri ↔ `GuiCore` bridge: the testable pieces of the read path.
//!
//! Three things live here, kept free of Tauri-runtime coupling so they unit-test
//! without spinning up a webview:
//!
//! - [`SharedVm`] — the current [`ViewModelSnapshot`] behind a mutex, shared
//!   between the poll thread (writer) and the [`get_view_model`] command (reader).
//! - [`step`] — drive the `GuiCore` through one whole poll cycle against an
//!   injected request sender, returning the resulting snapshot. The real poll
//!   thread injects the blocking socket client; tests inject a fake daemon.
//! - [`should_emit`] — the emit-on-change decision (full-VM, last-wins; never
//!   deltas), so identical snapshots do not spam the frontend with events.
//!
//! [`poll_loop`] composes them with the actual socket client, the `AppHandle`
//! event push, and the timer — that wiring is thin and, like the egui worker, is
//! not unit-tested (it has no decision logic).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tauri::{AppHandle, Emitter, State};

use splitway_shared::ipc::client::{self, ClientError};
use splitway_shared::ipc::{Request, Response};

use splitway_gui_core::{GuiCore, ViewModelSnapshot};

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
pub fn poll_loop(app: AppHandle, shared: SharedVm) {
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
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
