//! The single state-owner task. All "currently applied" state lives here and
//! is mutated only from this one task, which serializes every transition.
//! VPN events and IPC requests both arrive as [`StateCommand`]s over an
//! `mpsc` channel — no shared `Mutex`, so there are no lock-ordering or
//! poisoning bugs by construction.
//!
//! Blocking `DnsBackend` calls (they shell out to `resolvectl`) run on
//! `spawn_blocking` so a slow command never stalls the actor while it awaits;
//! because the actor awaits the result before taking the next command, state
//! transitions stay strictly serialized.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use splitway_shared::config::{self, LocalConfig, OpenVpnConfig};
use splitway_shared::ipc::{
    AppliedInfo, ConfigView, DetectorHealth, Request, Response, RoutingState, StatusInfo,
};
use splitway_shared::platform::{DnsBackend, PlatformError, VpnDetector, VpnEvent, VpnInfo};

use crate::interfaces::list_interfaces;

/// Builds the platform VPN detector for a config. Injected into the
/// [`StateMachine`] (like [`DnsBackend`]) so the re-arm lifecycle can be driven
/// with a mock detector in tests instead of touching the real platform.
pub trait DetectorFactory: Send + Sync {
    fn create(&self, config: &LocalConfig) -> Box<dyn VpnDetector>;
}

/// Production factory: the real per-platform detector selected by `vpn_backend`.
pub struct PlatformDetectorFactory;

impl DetectorFactory for PlatformDetectorFactory {
    fn create(&self, config: &LocalConfig) -> Box<dyn VpnDetector> {
        crate::detector::create_vpn_detector(config)
    }
}

/// Routine commands funneled into the state-owner task. Shutdown is delivered
/// out-of-band (see [`run_state`]) so it can preempt a backlog of these.
pub enum StateCommand {
    /// A VPN up/down event from the detector. `generation` identifies the watch
    /// that produced it: [`StateMachine::arm_watch`] bumps the generation on
    /// every (re-)arm, and a stale event from a torn-down watch is ignored (see
    /// [`StateMachine::on_vpn_event`]), so an interface switch never lets the old
    /// interface's last gasp move `vpn_up`.
    Vpn { generation: u64, event: VpnEvent },
    /// The forwarding task observed its detector's event stream end on its own
    /// (the watch task terminated — e.g. NetworkManager/D-Bus absent, so the
    /// async `watch()` succeeded but its spawned loop returned at once). Carries
    /// the watch generation so a stream torn down by a re-arm is ignored.
    WatchEnded { generation: u64 },
    /// An IPC request plus the channel to reply on.
    Ipc {
        request: Request,
        reply: oneshot::Sender<Response>,
    },
}

/// A snapshot of what is currently applied to the system. Includes the DNS
/// servers so that a VPN DNS rotation (same interface and domains, different
/// servers) is seen as out-of-sync and re-applied rather than treated as
/// already converged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    interface: String,
    domains: Vec<String>,
    dns_servers: Vec<String>,
}

pub struct StateMachine {
    backend: Arc<dyn DnsBackend>,
    /// Builds the VPN detector on every (re-)arm. Injected for testability.
    detector_factory: Arc<dyn DetectorFactory>,
    /// A clone of the actor's own command sender, handed to each re-armed
    /// forwarding task so it can feed `StateCommand::Vpn` back into the actor.
    state_tx: mpsc::Sender<StateCommand>,
    config: LocalConfig,
    config_path: PathBuf,
    vpn_up: bool,
    /// The most recent `Up` info, used to (re-)apply rules.
    last_info: Option<VpnInfo>,
    /// What is applied right now; `None` means reverted.
    applied: Option<Applied>,
    /// Interfaces whose rules a live switch could not revert, and which are no
    /// longer the configured interface — so the new interface's apply (which
    /// overwrites `applied`) would otherwise forget them. A later reconcile or
    /// shutdown keeps retrying their cleanup. Almost always empty.
    orphaned: Vec<String>,
    /// Set when the last apply/revert failed and left the real system state
    /// uncertain relative to `applied` (e.g. the Linux backend rolled the link
    /// back to clean on a domain-step failure, or a `revert` failed because the
    /// link had vanished). Forces the next reconcile to act even when the
    /// desired target equals the — now possibly stale — `applied` snapshot, so a
    /// post-failure "already converged" check can never skip a needed re-apply.
    needs_resync: bool,
    /// Cancel handle for the current watch's forwarding task. Aborting it drops
    /// the detector's `Receiver<VpnEvent>`, which closes the channel and lets the
    /// detector release its resources (see [`Self::arm_watch`]).
    watch_cancel: Option<AbortHandle>,
    /// Monotonic id of the current watch, bumped on every (re-)arm. Events from a
    /// superseded watch carry an old generation and are ignored.
    watch_generation: u64,
    /// Health of the current watch, set by [`Self::arm_watch`] and surfaced in
    /// [`Self::status`].
    detector_health: DetectorHealth,
}

impl StateMachine {
    pub fn new(
        backend: Arc<dyn DnsBackend>,
        detector_factory: Arc<dyn DetectorFactory>,
        config: LocalConfig,
        config_path: PathBuf,
        state_tx: mpsc::Sender<StateCommand>,
    ) -> Self {
        Self {
            backend,
            detector_factory,
            state_tx,
            config,
            config_path,
            vpn_up: false,
            last_info: None,
            applied: None,
            orphaned: Vec::new(),
            needs_resync: false,
            watch_cancel: None,
            watch_generation: 0,
            // No watch is armed until `arm_watch` runs (in `run_state`, before
            // the command loop). Inactive until then.
            detector_health: DetectorHealth::Inactive,
        }
    }

    /// (Re-)arm the VPN detector watch for the current config. Called once at
    /// startup (from [`run_state`]) and again whenever a config change touches
    /// the watch-affecting fields (see [`Self::adopt_config`]).
    ///
    /// In order: tear down the current watch (abort its forwarding task, which
    /// drops the detector's `Receiver<VpnEvent>` and closes the channel), bump
    /// the generation, then build and start the new watch.
    ///
    /// Teardown relies on each detector observing the closed channel:
    /// - **NetworkManager** and **standalone OpenVPN** `select!` on `tx.closed()`
    ///   at every await point, so the D-Bus connection / management socket is
    ///   released promptly when the receiver drops.
    /// - **macOS** (SCDynamicStore) is lazy by design — its CFRunLoop thread
    ///   notices the dropped receiver on the next network/DNS change (which an
    ///   interface switch reliably produces) and then stops; until then it sits
    ///   parked. No stale events reach the actor (the receiver is gone), and the
    ///   thread is reaped at process exit, so this is bounded, not a growing leak.
    ///
    /// There is **no separate post-arm `detect()` sample**: every detector emits
    /// its current state as the first streamed event (NM samples the device
    /// state on subscribe, OpenVPN issues `state`, macOS samples after arming),
    /// so a one-shot `detect()` here would double-apply. We rely on that first
    /// event to set `vpn_up` for the new interface.
    fn arm_watch(&mut self) {
        // Tear down the previous watch first. Aborting the forwarding task drops
        // its receiver; the detector then releases its resources as documented.
        if let Some(handle) = self.watch_cancel.take() {
            handle.abort();
        }
        // Any event still in flight from the old forwarding task carries the old
        // generation and is dropped by `on_vpn_event`.
        self.watch_generation = self.watch_generation.wrapping_add(1);
        let generation = self.watch_generation;

        let interface = self.config.vpn_name.clone();
        if interface.is_empty() {
            // No interface to watch: auto-apply stays off until one is set. This
            // is the intended startup-empty behaviour, not a failure.
            log::info!("no vpn_name configured; VPN watch is inactive");
            self.detector_health = DetectorHealth::Inactive;
            return;
        }

        let detector = self.detector_factory.create(&self.config);
        match detector.watch(&interface) {
            Ok(mut events) => {
                let state_tx = self.state_tx.clone();
                let handle = tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        if state_tx
                            .send(StateCommand::Vpn { generation, event })
                            .await
                            .is_err()
                        {
                            return; // the state task is gone; nothing to report to
                        }
                    }
                    // The stream ended on its own (the detector's watch task
                    // terminated). On a re-arm this task is aborted mid-await and
                    // never reaches here, so reaching it means an unexpected end —
                    // tell the actor so `detector_health` stops claiming Active.
                    log::debug!("VPN event stream for generation {generation} ended");
                    let _ = state_tx.send(StateCommand::WatchEnded { generation }).await;
                });
                self.watch_cancel = Some(handle.abort_handle());
                self.detector_health = DetectorHealth::Active;
                log::info!("armed VPN watch for {interface}");
            }
            Err(e) => {
                // Auto-apply is off, but IPC stays up. The interface that was
                // being watched before this arm was already reverted by the
                // caller's reconcile, so nothing is stranded.
                log::error!(
                    "failed to start VPN watch for {interface}: {e}; \
                     IPC is still available, auto-apply is not"
                );
                self.detector_health = DetectorHealth::Error(e.to_string());
            }
        }
    }

    /// What *should* be applied given the current config and VPN state.
    /// `None` means "nothing should be applied" (revert to direct DNS).
    ///
    /// An empty domain list yields `None`: there is nothing to route, and
    /// `resolvectl domain <iface>` with zero domains does not clear existing
    /// ones, so applying an empty set would leave stale split-DNS active.
    /// Removing the last domain therefore reverts instead.
    ///
    /// An Up with no pushed DNS servers also yields `None`: there is nowhere to
    /// route the domains to (a standalone OpenVPN whose `PUSH_REPLY` carried no
    /// `dhcp-option DNS`), so there is nothing to apply. Returning `None` rather
    /// than a do-nothing apply keeps `applied` unset, so a later down/disable/
    /// shutdown does not `resolvectl revert` per-link resolver state Splitway
    /// never set (e.g. one an OpenVPN up-script installed). If a *prior* session
    /// had DNS and the new one does not, this reverts that prior session's rules.
    ///
    /// The last event's interface must also match the configured `vpn_name`.
    /// A config change that switches `vpn_name` resets `last_info`/`vpn_up` and
    /// re-arms the watch (see [`Self::adopt_config`]), so the old interface is
    /// reverted and the new watch resamples; `last_info.interface_name` therefore
    /// matches `vpn_name` whenever the configured interface is up.
    fn desired(&self) -> Option<(VpnInfo, Vec<String>)> {
        let active = self.config.enabled && self.vpn_up && !self.config.vpn_hosts.is_empty();
        match &self.last_info {
            Some(info)
                if active
                    && !info.dns_servers.is_empty()
                    && info.interface_name == self.config.vpn_name =>
            {
                Some((info.clone(), self.config.vpn_hosts.clone()))
            }
            _ => None,
        }
    }

    /// Drive the system toward [`Self::desired`], then opportunistically retry
    /// cleanup of any interface orphaned by an earlier failed switch-revert. The
    /// primary reconcile outcome is what callers surface; the orphan cleanup is
    /// best-effort (and almost always a no-op — the list is empty).
    async fn reconcile(&mut self) -> Result<(), PlatformError> {
        let result = self.reconcile_primary().await;
        self.revert_orphaned().await;
        result
    }

    /// Best-effort cleanup of interfaces orphaned by a failed revert during a
    /// live switch (see [`Self::adopt_config`]): the new interface's successful
    /// apply overwrites `applied`, so a stale old interface is tracked in
    /// `orphaned` instead and reverted here whenever the backend recovers.
    /// Successes drop from the list; failures stay for the next attempt.
    async fn revert_orphaned(&mut self) {
        if self.orphaned.is_empty() {
            return;
        }
        for interface in std::mem::take(&mut self.orphaned) {
            let backend = self.backend.clone();
            let interface_for_revert = interface.clone();
            match tokio::task::spawn_blocking(move || backend.revert_rules(&interface_for_revert))
                .await
            {
                Ok(Ok(())) => {
                    log::info!("cleaned up orphaned interface {interface} after a switch")
                }
                Ok(Err(e)) => {
                    log::warn!("orphaned interface {interface} still needs cleanup: {e}");
                    self.orphaned.push(interface);
                }
                Err(e) => {
                    log::error!("orphan revert task panicked for {interface}: {e}");
                    self.orphaned.push(interface);
                }
            }
        }
    }

    /// Drive the system toward [`Self::desired`], applying or reverting only
    /// when reality differs from the goal (so it is idempotent and a no-op
    /// when already converged). Returns the backend outcome so callers can
    /// surface a failure instead of silently swallowing it.
    async fn reconcile_primary(&mut self) -> Result<(), PlatformError> {
        match self.desired() {
            Some((info, domains)) => {
                let target = Applied {
                    interface: info.interface_name.clone(),
                    domains,
                    dns_servers: info.dns_servers.clone(),
                };
                // A matching snapshot only means "already converged" when the
                // last apply/revert actually succeeded; after a failure the
                // snapshot may not reflect reality, so `needs_resync` forces the
                // re-apply through instead of trusting the stale equality.
                if !self.needs_resync && self.applied.as_ref() == Some(&target) {
                    return Ok(());
                }
                let backend = self.backend.clone();
                let info_for_apply = info.clone();
                let domains_for_apply = target.domains.clone();
                let result = tokio::task::spawn_blocking(move || {
                    backend.apply_rules(&info_for_apply, &domains_for_apply)
                })
                .await;
                match result {
                    Ok(Ok(())) => {
                        log::info!(
                            "applied rules on {} for {} domain(s)",
                            target.interface,
                            target.domains.len()
                        );
                        self.applied = Some(target);
                        self.needs_resync = false;
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        // A failed apply may have changed the system before
                        // failing: the backend can return Err after a partial
                        // change whose own rollback also failed (e.g. resolvectl
                        // `dns` set, `domain` failed, then `revert` failed),
                        // leaving the link half-configured — even on the *first*
                        // apply, when there is no previous snapshot. Record the
                        // attempted target as the cleanup state so a later
                        // down/disable/shutdown still reverts the interface, and
                        // set `needs_resync` so the next reconcile re-applies
                        // rather than trusting this now-uncertain snapshot as
                        // converged.
                        log::error!("apply_rules failed on {}: {e}", info.interface_name);
                        self.applied = Some(target);
                        self.needs_resync = true;
                        Err(e)
                    }
                    Err(e) => {
                        // Same reasoning as the error case: a panic mid-apply may
                        // have left the link partially configured, so record the
                        // target for cleanup rather than assuming it was untouched.
                        log::error!("apply task panicked: {e}");
                        self.applied = Some(target);
                        self.needs_resync = true;
                        Err(PlatformError::CommandFailed(format!(
                            "apply task panicked: {e}"
                        )))
                    }
                }
            }
            None => self.revert().await,
        }
    }

    /// Revert whatever is currently applied (no-op if nothing is). On failure
    /// `applied` is left set, so a later reconcile or shutdown retries it.
    async fn revert(&mut self) -> Result<(), PlatformError> {
        let Some(applied) = self.applied.clone() else {
            // Nothing recorded as applied: the system already matches the
            // "reverted" goal, so any prior uncertainty is resolved.
            self.needs_resync = false;
            return Ok(());
        };
        let backend = self.backend.clone();
        let interface = applied.interface.clone();
        let result = tokio::task::spawn_blocking(move || backend.revert_rules(&interface)).await;
        match result {
            Ok(Ok(())) => {
                log::info!("reverted rules on {}", applied.interface);
                self.applied = None;
                self.needs_resync = false;
                Ok(())
            }
            Ok(Err(e)) => {
                // The revert failed, so the link may still carry our rules (or
                // may have vanished, taking them with it — we cannot tell).
                // Keep `applied` so shutdown still retries, and mark
                // `needs_resync` so a later matching `Up` re-applies on the new
                // link instead of trusting the retained snapshot as converged.
                log::error!("revert_rules failed on {}: {e}", applied.interface);
                self.needs_resync = true;
                Err(e)
            }
            Err(e) => {
                log::error!("revert task panicked: {e}");
                self.needs_resync = true;
                Err(PlatformError::CommandFailed(format!(
                    "revert task panicked: {e}"
                )))
            }
        }
    }

    /// Entry point for a forwarded detector event. Drops events from a watch
    /// generation we have since superseded (an interface switch may leave an
    /// in-flight event from the old forwarding task), so the old interface's
    /// last gasp can never move `vpn_up` after a re-arm. Live events flow on to
    /// [`Self::on_event`].
    pub async fn on_vpn_event(&mut self, generation: u64, event: VpnEvent) {
        if generation != self.watch_generation {
            log::debug!(
                "ignoring VPN event from superseded watch (generation {generation}, current {})",
                self.watch_generation
            );
            return;
        }
        self.on_event(event).await;
    }

    /// Handle a forwarded [`StateCommand::WatchEnded`]. If it is for the *current*
    /// watch — not one already superseded by a re-arm — the detector terminated
    /// on its own, so no further VPN events can arrive and auto-apply is off;
    /// mark the detector unhealthy. A superseded generation is the expected
    /// teardown of a re-arm and is ignored.
    pub fn on_watch_ended(&mut self, generation: u64) {
        if generation != self.watch_generation {
            return;
        }
        log::warn!(
            "VPN watch for {} ended on its own; auto-apply is off until re-armed",
            self.config.vpn_name
        );
        self.detector_health = DetectorHealth::Error("watch stream ended".to_string());
    }

    pub async fn on_event(&mut self, event: VpnEvent) {
        match event {
            VpnEvent::Up(info) => {
                log::info!(
                    "VPN up: {} ({} DNS server(s))",
                    info.interface_name,
                    info.dns_servers.len()
                );
                self.vpn_up = true;
                self.last_info = Some(info);
            }
            VpnEvent::Down { interface_name } => {
                log::info!("VPN down: {interface_name}");
                self.vpn_up = false;
            }
        }
        // Event-driven reconcile is fire-and-forget; failures are logged
        // inside reconcile and retried on the next event.
        let _ = self.reconcile().await;
    }

    pub async fn on_request(&mut self, request: Request) -> Response {
        match request {
            Request::Status => Response::Status(self.status()),
            Request::Enable => self.set_enabled(true).await,
            Request::Disable => self.set_enabled(false).await,
            Request::AddDomain(domain) => self.add_domain(domain).await,
            Request::RemoveDomain(domain) => self.remove_domain(domain).await,
            Request::ListDomains => Response::Domains(self.config.vpn_hosts.clone()),
            Request::ReloadConfig => self.reload_config().await,
            Request::GetConfig => Response::Config(self.config_view()),
            Request::SetConfig(view) => self.set_config(view).await,
            Request::ListInterfaces => match list_interfaces() {
                Ok(interfaces) => Response::Interfaces(interfaces),
                // Enumeration failure is a clean error to the client, never a
                // panic — the GUI falls back to free-text entry.
                Err(e) => Response::Error(format!("failed to list interfaces: {e}")),
            },
        }
    }

    /// Revert active rules on shutdown so the system never stays
    /// half-configured after the daemon exits — both the currently-applied
    /// interface and any orphaned by a failed live switch. Returns `true` if the
    /// system is left clean, `false` if a revert failed and rules may remain.
    pub async fn shutdown(&mut self) -> bool {
        // Retry any orphaned-interface cleanup first, so a switch whose old
        // revert failed does not strand rules past shutdown.
        self.revert_orphaned().await;
        let applied_clean = if self.applied.is_none() {
            true
        } else {
            log::info!("shutdown: reverting active rules");
            match self.revert().await {
                Ok(()) => true,
                Err(e) => {
                    log::error!("shutdown: revert failed: {e}; system may be left half-configured");
                    false
                }
            }
        };
        if applied_clean && self.orphaned.is_empty() {
            log::info!("shutdown: system left clean");
            true
        } else {
            if !self.orphaned.is_empty() {
                log::error!(
                    "shutdown: {} interface(s) orphaned by a failed switch still need cleanup; \
                     system may be left half-configured",
                    self.orphaned.len()
                );
            }
            false
        }
    }

    fn status(&self) -> StatusInfo {
        StatusInfo {
            enabled: self.config.enabled,
            interface: self.config.vpn_name.clone(),
            vpn_up: self.vpn_up,
            // Map the private `Applied` snapshot to the wire projection: `None`
            // recovers the old "applied?" bool, and `Some` exposes the live
            // domain → DNS mapping for client-side verification.
            applied: self.applied.as_ref().map(|a| AppliedInfo {
                interface: a.interface.clone(),
                domains: a.domains.clone(),
                dns_servers: a.dns_servers.clone(),
            }),
            routing_state: self.routing_state(),
            detector_health: self.detector_health.clone(),
            domains: self.config.vpn_hosts.clone(),
        }
    }

    /// Summarize *why* routing is or is not active, mapped from the same inputs
    /// [`Self::desired`] uses plus `needs_resync`. Belief, not reality: it
    /// reports what the daemon intends, not a read-back of the live system.
    ///
    /// The branches mirror `desired()` in priority order (most fundamental
    /// first). There is deliberately no `InterfaceMismatch` variant: live re-arm
    /// resets `vpn_up`/`last_info` on a switch and only the configured
    /// interface's watch repopulates them, so `last_info.interface_name` always
    /// matches `config.vpn_name` while up; a stale mismatch (were it ever to
    /// occur) is reported as `NoDnsFromVpn` rather than a near-unreachable state.
    fn routing_state(&self) -> RoutingState {
        if !self.config.enabled {
            return RoutingState::Disabled;
        }
        if self.config.vpn_hosts.is_empty() {
            return RoutingState::NoDomains;
        }
        if !self.vpn_up {
            return RoutingState::VpnDown;
        }
        let has_vpn_dns = matches!(
            &self.last_info,
            Some(info)
                if !info.dns_servers.is_empty() && info.interface_name == self.config.vpn_name
        );
        if !has_vpn_dns {
            return RoutingState::NoDnsFromVpn;
        }
        // Up with usable DNS: rules should be applied. Distinguish a clean apply
        // from a pending/failed one (a failed apply/revert sets `needs_resync`).
        match (&self.applied, self.needs_resync) {
            (Some(_), false) => RoutingState::Applied,
            _ => RoutingState::ApplyFailed,
        }
    }

    async fn set_enabled(&mut self, enabled: bool) -> Response {
        if self.config.enabled == enabled {
            // No config change, but a previous apply/revert may have failed;
            // reconcile so a repeated enable/disable retries it instead of
            // reporting success while the system is still out of sync.
            return match self.reconcile().await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(format!("failed to apply current state: {e}")),
            };
        }
        let mut next = self.config.clone();
        next.enabled = enabled;
        self.commit(next).await
    }

    async fn add_domain(&mut self, domain: String) -> Response {
        if self.config.vpn_hosts.iter().any(|d| d == &domain) {
            return Response::Error(format!("domain already present: {domain}"));
        }
        let mut next = self.config.clone();
        next.vpn_hosts.push(domain);
        self.commit(next).await
    }

    async fn remove_domain(&mut self, domain: String) -> Response {
        if !self.config.vpn_hosts.iter().any(|d| d == &domain) {
            // Removing an absent domain is a no-op success.
            return Response::Ok;
        }
        let mut next = self.config.clone();
        next.vpn_hosts.retain(|d| d != &domain);
        self.commit(next).await
    }

    async fn reload_config(&mut self) -> Response {
        match config::load_config_from(&self.config_path) {
            Ok(next) => match self.adopt_config(next).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(format!("config reloaded, but applying it failed: {e}")),
            },
            Err(e) => Response::Error(format!("failed to reload config: {e}")),
        }
    }

    /// Build the editable config projection sent in reply to
    /// [`Request::GetConfig`]. `config_path` is the daemon's effective file
    /// path, informational only — [`Self::set_config`] ignores it.
    fn config_view(&self) -> ConfigView {
        ConfigView {
            vpn_name: self.config.vpn_name.clone(),
            vpn_backend: self.config.vpn_backend,
            openvpn_management: self.config.openvpn.management.clone(),
            openvpn_management_password_file: self.config.openvpn.management_password_file.clone(),
            config_path: self.config_path.display().to_string(),
        }
    }

    /// Apply a [`Request::SetConfig`] update. Overwrites only the editable
    /// projection's fields (`vpn_name`, `vpn_backend`, `openvpn.*`), preserving
    /// `enabled` and the domain list owned by the other verbs, then persists
    /// and reconciles through the single-writer [`Self::commit`] path. The
    /// incoming `config_path` is ignored: the active path is fixed at launch.
    async fn set_config(&mut self, view: ConfigView) -> Response {
        let mut next = self.config.clone();
        next.vpn_name = view.vpn_name;
        next.vpn_backend = view.vpn_backend;
        next.openvpn = OpenVpnConfig {
            management: view.openvpn_management,
            management_password_file: view.openvpn_management_password_file,
        };
        // Reject a known-invalid combination at the IPC boundary rather than
        // persisting a config that the OpenVPN detector will only fail on later:
        // the openvpn backend has no usable target without a management endpoint.
        if next.vpn_backend == config::VpnBackend::OpenVpn
            && next.openvpn.management.trim().is_empty()
        {
            return Response::Error(
                "invalid config: the openvpn backend requires a non-empty openvpn.management \
                 (host:port or a unix socket path)"
                    .to_string(),
            );
        }
        // A watch-affecting change (vpn_name / vpn_backend / openvpn) now takes
        // effect live: `commit` -> `adopt_config` re-arms the watch, so there is
        // no restart caveat to warn about anymore.
        self.commit(next).await
    }

    /// Persist `next` first; only adopt it in memory if the write succeeds,
    /// then reconcile (re-arming the watch if the watch-affecting fields
    /// changed — see [`Self::adopt_config`]). This keeps the in-memory config
    /// and disk in lockstep. A persisted change whose re-apply fails is reported
    /// as an error so the caller is not told "ok" while DNS is out of sync.
    async fn commit(&mut self, next: LocalConfig) -> Response {
        if let Err(e) = config::save_config_to(&self.config_path, &next) {
            return Response::Error(format!("failed to persist config: {e}"));
        }
        match self.adopt_config(next).await {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(format!("config saved, but applying it failed: {e}")),
        }
    }

    /// Adopt `next` as the live config and reconcile. When the watch-affecting
    /// fields (`vpn_name` / `vpn_backend` / `openvpn`) changed, this performs the
    /// live re-arm: reset the VPN state (`vpn_up` / `last_info`), reconcile —
    /// which now reverts the old interface, because the reset makes `desired()`
    /// return `None` — and only **then** arm the new watch. Reverting before
    /// arming preserves the "no half-configured state" guarantee across a switch.
    /// The new watch's first streamed event re-establishes `vpn_up` for the new
    /// interface (no separate sample — see [`Self::arm_watch`]).
    ///
    /// Returns the reconcile outcome; the watch is (re-)armed regardless of it,
    /// so a failed old-interface revert still does not block bringing the new
    /// watch up.
    async fn adopt_config(&mut self, next: LocalConfig) -> Result<(), PlatformError> {
        let rearm = watch_settings_changed(&self.config, &next);
        self.config = next;
        if rearm {
            // Forget the old interface's state so `desired()` -> `None` and the
            // reconcile below reverts it; the new watch will resample.
            self.vpn_up = false;
            self.last_info = None;
        }
        let result = self.reconcile().await;
        if rearm {
            // If reverting the old interface just failed, it is no longer the
            // interface we are about to watch/apply, so the new interface's
            // (successful) apply would overwrite `applied` and forget it. Hand it
            // to the orphaned list, which a later reconcile or shutdown retries —
            // otherwise a switch where old cleanup fails but new apply succeeds
            // would strand the old interface's split-DNS rules.
            let stale = match &self.applied {
                Some(applied) if applied.interface != self.config.vpn_name => {
                    Some(applied.interface.clone())
                }
                _ => None,
            };
            if let Some(interface) = stale {
                log::warn!(
                    "could not revert {} while switching to {}; tracking it for later cleanup",
                    interface,
                    self.config.vpn_name
                );
                if !self.orphaned.contains(&interface) {
                    self.orphaned.push(interface);
                }
                // Let the new interface own `applied`/`needs_resync` from here.
                self.applied = None;
                self.needs_resync = false;
            }
            self.arm_watch();
        }
        result
    }
}

/// Whether a config delta requires re-arming the detector watch — i.e. it
/// touches a field the watch is keyed on. Domain/`enabled` edits do not (those
/// only change `desired()`), so they reconcile without tearing the watch down.
fn watch_settings_changed(old: &LocalConfig, new: &LocalConfig) -> bool {
    old.vpn_name != new.vpn_name || old.vpn_backend != new.vpn_backend || old.openvpn != new.openvpn
}

/// The state-owner task loop. Owns the [`StateMachine`] outright.
///
/// `shutdown` carries the reply channel for the shutdown ack. It is selected
/// `biased`, ahead of routine commands, so the revert preempts any backlog of
/// queued VPN events / IPC requests rather than waiting behind them. The ack
/// reports whether the system was left clean.
pub async fn run_state(
    mut machine: StateMachine,
    mut rx: mpsc::Receiver<StateCommand>,
    mut shutdown: oneshot::Receiver<oneshot::Sender<bool>>,
) {
    // Arm the VPN watch once here, before the command loop, so all watch
    // lifecycle (start at boot, re-arm on config change) lives in one owner —
    // the actor — rather than being split with `run_async`.
    machine.arm_watch();
    loop {
        tokio::select! {
            biased;

            ack = &mut shutdown => {
                let clean = machine.shutdown().await;
                if let Ok(ack_tx) = ack {
                    let _ = ack_tx.send(clean);
                }
                break;
            }

            command = rx.recv() => {
                match command {
                    Some(StateCommand::Vpn { generation, event }) => {
                        machine.on_vpn_event(generation, event).await
                    }
                    Some(StateCommand::WatchEnded { generation }) => {
                        machine.on_watch_ended(generation)
                    }
                    Some(StateCommand::Ipc { request, reply }) => {
                        let response = machine.on_request(request).await;
                        // The client may have hung up; that is fine.
                        let _ = reply.send(response);
                    }
                    None => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Records what the state machine asks the backend to do. `fail_apply` /
    /// `fail_revert` are atomic so a test can flip them after a first call.
    #[derive(Default)]
    struct MockBackend {
        applies: Mutex<Vec<(String, Vec<String>)>>,
        reverts: Mutex<Vec<String>>,
        fail_apply: AtomicBool,
        fail_revert: AtomicBool,
    }

    impl MockBackend {
        fn set_fail_apply(&self, fail: bool) {
            self.fail_apply.store(fail, Ordering::Relaxed);
        }

        fn set_fail_revert(&self, fail: bool) {
            self.fail_revert.store(fail, Ordering::Relaxed);
        }
    }

    impl DnsBackend for MockBackend {
        fn apply_rules(&self, info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError> {
            if self.fail_apply.load(Ordering::Relaxed) {
                return Err(PlatformError::CommandFailed(
                    "mock apply failure".to_string(),
                ));
            }
            self.applies
                .lock()
                .unwrap()
                .push((info.interface_name.clone(), domains.to_vec()));
            Ok(())
        }

        fn revert_rules(&self, interface: &str) -> Result<(), PlatformError> {
            self.reverts.lock().unwrap().push(interface.to_string());
            if self.fail_revert.load(Ordering::Relaxed) {
                return Err(PlatformError::CommandFailed(
                    "mock revert failure".to_string(),
                ));
            }
            Ok(())
        }

        fn status(&self, _interface: &str) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// A detector factory that never arms a live watch — its `watch` returns an
    /// already-closed stream. Used by the transition/IPC tests, which drive
    /// `on_event`/`on_request` directly and only incidentally re-arm (a config
    /// change), so they need an arming target that produces no events.
    struct NoopDetectorFactory;

    impl DetectorFactory for NoopDetectorFactory {
        fn create(&self, _config: &LocalConfig) -> Box<dyn VpnDetector> {
            Box::new(NoopDetector)
        }
    }

    struct NoopDetector;

    impl VpnDetector for NoopDetector {
        fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
            Err(PlatformError::VpnNotFound(interface.to_string()))
        }

        fn watch(
            &self,
            _interface: &str,
        ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
            // Drop the sender: an idle, immediately-closed stream.
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
    }

    /// Shared state for [`MockDetectorFactory`]: which interfaces' `watch` should
    /// fail (return `Err`), which should arm but have their stream end at once
    /// (return `Ok` then close — like NM returning the receiver before a failed
    /// D-Bus connect), and a record of every watch armed (so a test can assert
    /// the old watch was torn down on re-arm).
    #[derive(Default)]
    struct MockDetectorShared {
        fail: Mutex<HashSet<String>>,
        die: Mutex<HashSet<String>>,
        watches: Mutex<Vec<Arc<WatchRecord>>>,
    }

    /// One armed mock watch. `stopped` flips to `true` when the forwarding task
    /// is aborted and the receiver drops — mirroring how a real detector
    /// observes `tx.closed()` and releases its resources.
    struct WatchRecord {
        interface: String,
        stopped: AtomicBool,
    }

    /// A scriptable detector for the re-arm tests. Each `watch` emits the current
    /// state as its first event (an `Up` with DNS — like the real detectors'
    /// post-arm sample), then idles until the receiver drops, recording the
    /// teardown. A configured interface fails to arm.
    struct MockDetectorFactory {
        shared: Arc<MockDetectorShared>,
    }

    impl DetectorFactory for MockDetectorFactory {
        fn create(&self, _config: &LocalConfig) -> Box<dyn VpnDetector> {
            Box::new(MockDetector {
                shared: self.shared.clone(),
            })
        }
    }

    struct MockDetector {
        shared: Arc<MockDetectorShared>,
    }

    impl VpnDetector for MockDetector {
        fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
            Ok(VpnInfo {
                interface_name: interface.to_string(),
                dns_servers: vec!["10.0.0.1".to_string()],
            })
        }

        fn watch(
            &self,
            interface: &str,
        ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
            if self.shared.fail.lock().unwrap().contains(interface) {
                return Err(PlatformError::CommandFailed(format!(
                    "mock watch failed for {interface}"
                )));
            }
            if self.shared.die.lock().unwrap().contains(interface) {
                // Arm successfully, then immediately close the stream (drop the
                // sender): the async watch "succeeded" but produces no events.
                let (_tx, rx) = mpsc::channel(1);
                return Ok(rx);
            }
            let record = Arc::new(WatchRecord {
                interface: interface.to_string(),
                stopped: AtomicBool::new(false),
            });
            self.shared.watches.lock().unwrap().push(record.clone());

            let (tx, rx) = mpsc::channel(8);
            let iface = interface.to_string();
            tokio::spawn(async move {
                // Emit the current state as the first event (the post-arm sample
                // the real detectors stream), so `vpn_up` tracks the new
                // interface without a separate `detect()`.
                let _ = tx
                    .send(VpnEvent::Up(VpnInfo {
                        interface_name: iface,
                        dns_servers: vec!["10.0.0.1".to_string()],
                    }))
                    .await;
                // Then release on receiver drop, like NM/OpenVPN's `tx.closed()`.
                tx.closed().await;
                record.stopped.store(true, Ordering::SeqCst);
            });
            Ok(rx)
        }
    }

    /// Process the next forwarded command (bounded wait), simulating the
    /// `run_state` loop's `Vpn`/`WatchEnded` arms so the re-arm tests can drive
    /// the watch's streamed sample and stream-end signal through the machine
    /// without spawning `run_state`. Returns `false` on timeout/closed.
    async fn pump_one_command(
        sm: &mut StateMachine,
        rx: &mut mpsc::Receiver<StateCommand>,
    ) -> bool {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(StateCommand::Vpn { generation, event })) => {
                sm.on_vpn_event(generation, event).await;
                true
            }
            Ok(Some(StateCommand::WatchEnded { generation })) => {
                sm.on_watch_ended(generation);
                true
            }
            _ => false,
        }
    }

    /// Poll `predicate` until true or a generous timeout elapses. Used to await
    /// an asynchronous teardown (the old watch observing its closed channel).
    async fn wait_until(predicate: impl Fn() -> bool) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition was not met within the timeout");
    }

    fn temp_config_path(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("splitway-state-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    fn config(enabled: bool, hosts: &[&str]) -> LocalConfig {
        LocalConfig {
            vpn_name: "wg0".to_string(),
            vpn_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            enabled,
            vpn_backend: config::VpnBackend::default(),
            openvpn: config::OpenVpnConfig::default(),
        }
    }

    fn vpn_up(interface: &str) -> VpnEvent {
        VpnEvent::Up(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers: vec!["10.0.0.1".to_string()],
        })
    }

    fn machine(backend: Arc<MockBackend>, cfg: LocalConfig, tag: &str) -> StateMachine {
        // A throwaway command sender: these tests drive `on_event`/`on_request`
        // directly and never consume forwarded events, so the receiver is
        // dropped. The Noop factory never produces events anyway.
        let (state_tx, _state_rx) = mpsc::channel(16);
        StateMachine::new(
            backend,
            Arc::new(NoopDetectorFactory),
            cfg,
            temp_config_path(tag),
            state_tx,
        )
    }

    /// Build a machine wired to a [`MockDetectorFactory`], keeping the command
    /// receiver so the re-arm tests can pump the watch's forwarded events
    /// (see [`pump_one_command`]).
    fn rearm_machine(
        backend: Arc<MockBackend>,
        shared: Arc<MockDetectorShared>,
        cfg: LocalConfig,
        tag: &str,
    ) -> (StateMachine, mpsc::Receiver<StateCommand>) {
        let (state_tx, state_rx) = mpsc::channel(16);
        let factory = Arc::new(MockDetectorFactory { shared });
        let sm = StateMachine::new(backend, factory, cfg, temp_config_path(tag), state_tx);
        (sm, state_rx)
    }

    #[tokio::test]
    async fn enabled_and_up_applies() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com", "b.com"]),
            "up-applies",
        );

        sm.on_event(vpn_up("wg0")).await;

        let applies = backend.applies.lock().unwrap();
        assert_eq!(applies.len(), 1);
        assert_eq!(applies[0].0, "wg0");
        assert_eq!(applies[0].1, vec!["a.com", "b.com"]);
        assert!(sm.applied.is_some());
    }

    #[tokio::test]
    async fn disabled_at_startup_does_not_apply() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(false, &["a.com"]),
            "disabled-no-apply",
        );

        sm.on_event(vpn_up("wg0")).await;

        assert!(backend.applies.lock().unwrap().is_empty());
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn down_reverts() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "down-reverts");

        sm.on_event(vpn_up("wg0")).await;
        sm.on_event(VpnEvent::Down {
            interface_name: "wg0".to_string(),
        })
        .await;

        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn dns_server_change_reapplies() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "dns-rotate");

        sm.on_event(vpn_up("wg0")).await; // applies with 10.0.0.1
                                          // Same interface and domains, but the VPN's DNS server rotated: this is
                                          // not "already converged" — the rules must be re-applied.
        sm.on_event(VpnEvent::Up(VpnInfo {
            interface_name: "wg0".to_string(),
            dns_servers: vec!["10.9.9.9".to_string()],
        }))
        .await;

        assert_eq!(
            backend.applies.lock().unwrap().len(),
            2,
            "a DNS server change must trigger a re-apply"
        );
    }

    #[tokio::test]
    async fn disable_request_reverts() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "disable-reverts");

        sm.on_event(vpn_up("wg0")).await;
        let resp = sm.on_request(Request::Disable).await;

        assert_eq!(resp, Response::Ok);
        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
        // Disable is persisted.
        let saved = config::load_config_from(&sm.config_path).unwrap();
        assert!(!saved.enabled);
    }

    #[tokio::test]
    async fn add_domain_while_up_reapplies() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "add-reapplies");

        sm.on_event(vpn_up("wg0")).await;
        let resp = sm.on_request(Request::AddDomain("b.com".to_string())).await;

        assert_eq!(resp, Response::Ok);
        let applies = backend.applies.lock().unwrap();
        assert_eq!(applies.len(), 2);
        assert_eq!(applies[1].1, vec!["a.com", "b.com"]);
        // Persisted.
        let saved = config::load_config_from(&sm.config_path).unwrap();
        assert_eq!(saved.vpn_hosts, vec!["a.com", "b.com"]);
    }

    #[tokio::test]
    async fn add_duplicate_domain_is_rejected() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "add-dup");

        sm.on_event(vpn_up("wg0")).await;
        let resp = sm.on_request(Request::AddDomain("a.com".to_string())).await;

        assert!(matches!(resp, Response::Error(_)));
        // No re-apply happened.
        assert_eq!(backend.applies.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn remove_absent_domain_is_noop_success() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "remove-absent");

        sm.on_event(vpn_up("wg0")).await;
        let applies_before = backend.applies.lock().unwrap().len();
        let resp = sm
            .on_request(Request::RemoveDomain("z.com".to_string()))
            .await;

        assert_eq!(resp, Response::Ok);
        // No re-apply for an absent removal.
        assert_eq!(backend.applies.lock().unwrap().len(), applies_before);
    }

    #[tokio::test]
    async fn empty_domain_list_does_not_apply() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &[]), "empty-no-apply");

        sm.on_event(vpn_up("wg0")).await;

        // Nothing to route → nothing applied (an empty `resolvectl domain`
        // would not clear anything anyway).
        assert!(backend.applies.lock().unwrap().is_empty());
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn up_with_no_dns_does_not_apply_or_revert() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "up-no-dns");

        // A standalone OpenVPN up whose PUSH_REPLY carried no DNS: there is
        // nowhere to route, so nothing is applied — and crucially nothing is
        // marked applied, so no later `resolvectl revert` runs against
        // resolver state Splitway never set.
        sm.on_event(VpnEvent::Up(VpnInfo {
            interface_name: "wg0".to_string(),
            dns_servers: Vec::new(),
        }))
        .await;

        assert!(backend.applies.lock().unwrap().is_empty());
        assert!(backend.reverts.lock().unwrap().is_empty());
        assert!(sm.applied.is_none());

        // A following Down must likewise not revert (nothing was ever applied).
        sm.on_event(VpnEvent::Down {
            interface_name: "wg0".to_string(),
        })
        .await;
        assert!(backend.reverts.lock().unwrap().is_empty());
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn up_losing_dns_reverts_prior_rules() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "up-loses-dns");

        // First session pushes DNS and applies.
        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());

        // The session re-pushes with no DNS (or a new no-DNS session on the same
        // interface): the prior split-DNS now points at gone servers, so it must
        // be reverted rather than left stale.
        sm.on_event(VpnEvent::Up(VpnInfo {
            interface_name: "wg0".to_string(),
            dns_servers: Vec::new(),
        }))
        .await;

        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn removing_last_domain_reverts() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "remove-last");

        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());

        let resp = sm
            .on_request(Request::RemoveDomain("a.com".to_string()))
            .await;

        assert_eq!(resp, Response::Ok);
        // The last domain is gone → revert rather than apply an empty set.
        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn reload_changing_interface_reverts_old() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "reload-iface");

        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());

        // Operator changes the configured interface and reloads.
        let new_cfg = LocalConfig {
            vpn_name: "wg1".to_string(),
            vpn_hosts: vec!["a.com".to_string()],
            enabled: true,
            vpn_backend: config::VpnBackend::default(),
            openvpn: config::OpenVpnConfig::default(),
        };
        config::save_config_to(&sm.config_path, &new_cfg).unwrap();
        let resp = sm.on_request(Request::ReloadConfig).await;

        assert_eq!(resp, Response::Ok);
        // The old interface's rules are reverted before the new watch is armed.
        // Nothing is applied to the new interface in this test because its watch
        // produces no events here (the live-switch apply on the new interface is
        // covered by the mock-detector re-arm tests below).
        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn shutdown_reverts_applied_rules() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "shutdown-reverts",
        );

        sm.on_event(vpn_up("wg0")).await;
        let clean = sm.shutdown().await;

        assert!(clean);
        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn status_reflects_state() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "status");

        sm.on_event(vpn_up("wg0")).await;
        let Response::Status(info) = sm.on_request(Request::Status).await else {
            panic!("expected Status response");
        };
        assert!(info.enabled);
        assert!(info.vpn_up);
        // `applied` is now the wire mapping: Some when rules are applied, with
        // the interface / domains / DNS the daemon believes it installed.
        let applied = info.applied.expect("rules should be applied");
        assert_eq!(applied.interface, "wg0");
        assert_eq!(applied.domains, vec!["a.com"]);
        assert_eq!(applied.dns_servers, vec!["10.0.0.1"]);
        assert_eq!(info.routing_state, RoutingState::Applied);
        assert_eq!(info.interface, "wg0");
        assert_eq!(info.domains, vec!["a.com"]);
    }

    #[tokio::test]
    async fn failed_first_apply_records_a_cleanup_target() {
        let backend = Arc::new(MockBackend {
            fail_apply: AtomicBool::new(true),
            ..Default::default()
        });
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "apply-fails");

        sm.on_event(vpn_up("wg0")).await;

        // Even on the first apply, the backend may have changed the system
        // before returning Err (e.g. resolvectl `dns` set, `domain` failed, and
        // the rollback also failed). The machine records the interface as
        // needing cleanup rather than assuming the failed apply left the system
        // untouched, so a later down/disable/shutdown still reverts it.
        assert!(sm.applied.is_some());

        backend.set_fail_apply(false);
        sm.on_event(VpnEvent::Down {
            interface_name: "wg0".to_string(),
        })
        .await;
        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn failed_reapply_preserves_previous_applied_state() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "reapply-fails");

        // First apply succeeds.
        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());

        // A re-apply (triggered by adding a domain) now fails...
        backend.set_fail_apply(true);
        let resp = sm.on_request(Request::AddDomain("b.com".to_string())).await;
        assert!(matches!(resp, Response::Error(_)));

        // ...the previous applied snapshot is retained (the old rules may
        // still be installed), so a later revert still runs against it.
        assert!(sm.applied.is_some());
        backend.set_fail_apply(false);
        sm.on_event(VpnEvent::Down {
            interface_name: "wg0".to_string(),
        })
        .await;
        assert_eq!(backend.reverts.lock().unwrap().as_slice(), &["wg0"]);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn shutdown_revert_failure_reports_unclean() {
        let backend = Arc::new(MockBackend {
            fail_revert: AtomicBool::new(true),
            ..Default::default()
        });
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "shutdown-revert-fails",
        );

        sm.on_event(vpn_up("wg0")).await;
        let clean = sm.shutdown().await;

        // Revert failed: shutdown reports unclean and keeps `applied` set.
        assert!(!clean);
        assert!(sm.applied.is_some());
    }

    #[tokio::test]
    async fn repeated_disable_retries_a_failed_revert() {
        let backend = Arc::new(MockBackend {
            fail_revert: AtomicBool::new(true),
            ..Default::default()
        });
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "disable-retry");

        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());

        // First disable persists enabled=false but the revert fails.
        let first = sm.on_request(Request::Disable).await;
        assert!(matches!(first, Response::Error(_)));
        assert!(sm.applied.is_some());

        // The backend recovers; a repeated `disable` (config unchanged) must
        // still reconcile and retry the revert rather than reporting success.
        backend.set_fail_revert(false);
        let second = sm.on_request(Request::Disable).await;
        assert_eq!(second, Response::Ok);
        assert!(sm.applied.is_none());
    }

    #[tokio::test]
    async fn config_mutation_reports_apply_failure_but_still_persists() {
        let backend = Arc::new(MockBackend {
            fail_apply: AtomicBool::new(true),
            ..Default::default()
        });
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "commit-apply-fails",
        );

        sm.on_event(vpn_up("wg0")).await; // first apply fails
        let resp = sm.on_request(Request::AddDomain("b.com".to_string())).await;

        // The re-apply fails, so the caller is told so rather than "ok"...
        assert!(matches!(resp, Response::Error(_)));
        // ...but the config change is still persisted to disk.
        let saved = config::load_config_from(&sm.config_path).unwrap();
        assert_eq!(saved.vpn_hosts, vec!["a.com", "b.com"]);
    }

    #[tokio::test]
    async fn reapply_after_failed_apply_even_when_target_matches_stale_snapshot() {
        // Regression: a failed re-apply (which a real backend rolls back, so the
        // link is left clean) must not strand the system. If a later change
        // makes the desired target equal the pre-failure `applied` snapshot, the
        // machine must still re-apply rather than treat it as already converged.
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "resync-after-apply",
        );

        // First apply succeeds: applied = {wg0, [a.com], [10.0.0.1]}.
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(backend.applies.lock().unwrap().len(), 1);

        // Add a domain; the re-apply fails (the backend would have rolled the
        // link back to clean, but `applied` still names the old snapshot).
        backend.set_fail_apply(true);
        let resp = sm.on_request(Request::AddDomain("b.com".to_string())).await;
        assert!(matches!(resp, Response::Error(_)));

        // Remove the just-added domain so the desired target equals the OLD
        // snapshot again. The prior failure must force a re-apply.
        backend.set_fail_apply(false);
        let resp = sm
            .on_request(Request::RemoveDomain("b.com".to_string()))
            .await;
        assert_eq!(resp, Response::Ok);
        assert_eq!(
            backend.applies.lock().unwrap().len(),
            2,
            "a target equal to a stale post-failure snapshot must still re-apply"
        );
        assert!(sm.applied.is_some());
    }

    #[tokio::test]
    async fn reapply_on_reconnect_after_a_failed_revert() {
        // Regression: a `Down` whose `revert` fails (e.g. the link already
        // vanished) keeps `applied` set so shutdown can retry. When the VPN
        // reconnects with identical params, the new link carries none of our
        // rules — so the matching snapshot must not be treated as converged; the
        // machine must re-apply.
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "resync-after-revert",
        );

        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(backend.applies.lock().unwrap().len(), 1);

        // Down with a failing revert: the snapshot is retained.
        backend.set_fail_revert(true);
        sm.on_event(VpnEvent::Down {
            interface_name: "wg0".to_string(),
        })
        .await;
        assert!(sm.applied.is_some());

        // Reconnect with the same interface, domains, and DNS servers.
        backend.set_fail_revert(false);
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(
            backend.applies.lock().unwrap().len(),
            2,
            "a reconnect after a failed revert must re-apply on the new link"
        );
        assert!(sm.applied.is_some());
    }

    #[tokio::test]
    async fn get_config_returns_editable_projection() {
        let backend = Arc::new(MockBackend::default());
        let mut cfg = config(true, &["a.com"]);
        cfg.vpn_backend = config::VpnBackend::OpenVpn;
        cfg.openvpn = config::OpenVpnConfig {
            management: "127.0.0.1:7505".to_string(),
            management_password_file: Some("/etc/mgmt.pass".to_string()),
        };
        let mut sm = machine(backend.clone(), cfg, "get-config");

        let Response::Config(view) = sm.on_request(Request::GetConfig).await else {
            panic!("expected Config response");
        };
        assert_eq!(view.vpn_name, "wg0");
        assert_eq!(view.vpn_backend, config::VpnBackend::OpenVpn);
        assert_eq!(view.openvpn_management, "127.0.0.1:7505");
        assert_eq!(
            view.openvpn_management_password_file.as_deref(),
            Some("/etc/mgmt.pass")
        );
        // The daemon reports its effective config path, informational only.
        assert_eq!(view.config_path, sm.config_path.display().to_string());
    }

    #[tokio::test]
    async fn set_config_updates_fields_persists_and_preserves_domains_and_enabled() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com", "b.com"]),
            "set-config",
        );

        let view = ConfigView {
            vpn_name: "tun9".to_string(),
            vpn_backend: config::VpnBackend::OpenVpn,
            openvpn_management: "/run/ovpn.sock".to_string(),
            openvpn_management_password_file: None,
            // A path here must be ignored: the active file is fixed at launch.
            config_path: "/ignored/by/the/daemon.json".to_string(),
        };
        let resp = sm.on_request(Request::SetConfig(view)).await;
        assert_eq!(resp, Response::Ok);

        // The editable fields are updated...
        assert_eq!(sm.config.vpn_name, "tun9");
        assert_eq!(sm.config.vpn_backend, config::VpnBackend::OpenVpn);
        assert_eq!(sm.config.openvpn.management, "/run/ovpn.sock");
        assert!(sm.config.openvpn.management_password_file.is_none());
        // ...the domain list and `enabled` (owned by the other verbs) survive...
        assert_eq!(sm.config.vpn_hosts, vec!["a.com", "b.com"]);
        assert!(sm.config.enabled);
        // ...and the change is persisted to the active file (whose path the
        // incoming view did not alter).
        let saved = config::load_config_from(&sm.config_path).unwrap();
        assert_eq!(saved.vpn_name, "tun9");
        assert_eq!(saved.vpn_hosts, vec!["a.com", "b.com"]);
        assert!(saved.enabled);
        assert_eq!(saved.openvpn.management, "/run/ovpn.sock");
    }

    #[tokio::test]
    async fn set_config_rejects_openvpn_without_management() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "set-config-invalid",
        );

        let view = ConfigView {
            vpn_name: "tun0".to_string(),
            vpn_backend: config::VpnBackend::OpenVpn,
            openvpn_management: "   ".to_string(), // empty/whitespace → unusable
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        let resp = sm.on_request(Request::SetConfig(view)).await;

        // Rejected at the boundary before commit: nothing is adopted in memory
        // (and nothing is persisted, since commit never runs).
        assert!(matches!(resp, Response::Error(_)));
        assert_eq!(sm.config.vpn_backend, config::VpnBackend::NetworkManager);
        assert!(sm.config.openvpn.management.is_empty());
    }

    // --- Phase 5: re-arm decision, routing-state mapping, generation guard ---

    #[test]
    fn watch_settings_changed_detects_only_watch_fields() {
        let base = config(true, &["a.com"]);
        assert!(!watch_settings_changed(&base, &base.clone()));

        // Domain / `enabled` edits do NOT require a re-arm (the watch is not
        // keyed on them — they only change `desired()`).
        let mut more_domains = base.clone();
        more_domains.vpn_hosts.push("b.com".to_string());
        assert!(!watch_settings_changed(&base, &more_domains));
        let mut disabled = base.clone();
        disabled.enabled = false;
        assert!(!watch_settings_changed(&base, &disabled));

        // vpn_name / vpn_backend / openvpn changes DO.
        let mut iface = base.clone();
        iface.vpn_name = "wg1".to_string();
        assert!(watch_settings_changed(&base, &iface));
        let mut backend = base.clone();
        backend.vpn_backend = config::VpnBackend::OpenVpn;
        assert!(watch_settings_changed(&base, &backend));
        let mut ovpn = base.clone();
        ovpn.openvpn.management = "127.0.0.1:7505".to_string();
        assert!(watch_settings_changed(&base, &ovpn));
    }

    #[tokio::test]
    async fn routing_state_maps_each_branch() {
        // Disabled.
        let sm = machine(
            Arc::new(MockBackend::default()),
            config(false, &["a.com"]),
            "rs-disabled",
        );
        assert_eq!(sm.routing_state(), RoutingState::Disabled);

        // No domains configured.
        let sm = machine(
            Arc::new(MockBackend::default()),
            config(true, &[]),
            "rs-nodomains",
        );
        assert_eq!(sm.routing_state(), RoutingState::NoDomains);

        // Enabled + domains but the VPN is not up.
        let sm = machine(
            Arc::new(MockBackend::default()),
            config(true, &["a.com"]),
            "rs-down",
        );
        assert_eq!(sm.routing_state(), RoutingState::VpnDown);

        // Up, but the VPN pushed no DNS.
        let mut sm = machine(
            Arc::new(MockBackend::default()),
            config(true, &["a.com"]),
            "rs-nodns",
        );
        sm.on_event(VpnEvent::Up(VpnInfo {
            interface_name: "wg0".to_string(),
            dns_servers: Vec::new(),
        }))
        .await;
        assert_eq!(sm.routing_state(), RoutingState::NoDnsFromVpn);

        // Up with DNS and rules applied.
        let mut sm = machine(
            Arc::new(MockBackend::default()),
            config(true, &["a.com"]),
            "rs-applied",
        );
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(sm.routing_state(), RoutingState::Applied);

        // Up with DNS but the apply failed (needs_resync).
        let backend = Arc::new(MockBackend {
            fail_apply: AtomicBool::new(true),
            ..Default::default()
        });
        let mut sm = machine(backend, config(true, &["a.com"]), "rs-failed");
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(sm.routing_state(), RoutingState::ApplyFailed);
    }

    #[tokio::test]
    async fn on_vpn_event_ignores_superseded_generation() {
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "gen-guard");

        // `watch_generation` starts at 0 in this harness (no `arm_watch`). An
        // event tagged with a different generation — a torn-down watch's last
        // gasp — is dropped, so it can never move `vpn_up`.
        sm.on_vpn_event(99, vpn_up("wg0")).await;
        assert!(!sm.vpn_up);
        assert!(backend.applies.lock().unwrap().is_empty());

        // An event from the current generation is processed normally.
        sm.on_vpn_event(0, vpn_up("wg0")).await;
        assert!(sm.vpn_up);
        assert_eq!(backend.applies.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rearm_switches_interface_reverts_old_and_applies_new() {
        let backend = Arc::new(MockBackend::default());
        let shared = Arc::new(MockDetectorShared::default());
        let (mut sm, mut rx) = rearm_machine(
            backend.clone(),
            shared.clone(),
            config(true, &["a.com"]),
            "rearm-switch",
        );

        // Startup arm (generation 1) for wg0; its streamed sample applies on wg0.
        sm.arm_watch();
        assert!(
            pump_one_command(&mut sm, &mut rx).await,
            "wg0 sample expected"
        );
        {
            let applies = backend.applies.lock().unwrap();
            assert_eq!(applies.len(), 1);
            assert_eq!(applies[0].0, "wg0");
        }
        assert!(sm.vpn_up);

        // Switch the configured interface to wg1.
        let view = ConfigView {
            vpn_name: "wg1".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        assert_eq!(sm.on_request(Request::SetConfig(view)).await, Response::Ok);

        // The old interface was reverted before the new watch armed.
        assert!(backend.reverts.lock().unwrap().contains(&"wg0".to_string()));

        // The new watch (generation 2) streams its sample → applies on wg1.
        assert!(
            pump_one_command(&mut sm, &mut rx).await,
            "wg1 sample expected"
        );
        assert!(backend
            .applies
            .lock()
            .unwrap()
            .iter()
            .any(|(iface, _)| iface == "wg1"));

        // vpn_up + status now track the new interface.
        let Response::Status(info) = sm.on_request(Request::Status).await else {
            panic!("expected Status");
        };
        assert!(info.vpn_up);
        assert_eq!(info.applied.as_ref().unwrap().interface, "wg1");
        assert_eq!(info.detector_health, DetectorHealth::Active);
        assert_eq!(info.routing_state, RoutingState::Applied);

        // The old wg0 watch was torn down (it observed the closed channel).
        let wg0 = shared
            .watches
            .lock()
            .unwrap()
            .iter()
            .find(|w| w.interface == "wg0")
            .cloned()
            .expect("wg0 watch recorded");
        wait_until(|| wg0.stopped.load(Ordering::SeqCst)).await;
    }

    #[tokio::test]
    async fn rearm_failure_sets_detector_error_keeps_ipc_and_reverts_old() {
        let backend = Arc::new(MockBackend::default());
        let shared = Arc::new(MockDetectorShared::default());
        // The new interface's watch will fail to start.
        shared.fail.lock().unwrap().insert("tun9".to_string());
        let (mut sm, mut rx) = rearm_machine(
            backend.clone(),
            shared.clone(),
            config(true, &["a.com"]),
            "rearm-fail",
        );

        // Startup arm + sample on wg0.
        sm.arm_watch();
        assert!(pump_one_command(&mut sm, &mut rx).await);
        assert_eq!(backend.applies.lock().unwrap().len(), 1);

        // Switch to tun9, whose watch cannot start.
        let view = ConfigView {
            vpn_name: "tun9".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        // The save + old-interface revert succeeded, so SetConfig returns Ok; the
        // arm failure is surfaced via detector_health, not by failing the save.
        assert_eq!(sm.on_request(Request::SetConfig(view)).await, Response::Ok);
        assert!(backend.reverts.lock().unwrap().contains(&"wg0".to_string()));

        // IPC still answers, reporting the failed detector and no apply on tun9.
        let Response::Status(info) = sm.on_request(Request::Status).await else {
            panic!("expected Status");
        };
        assert!(matches!(info.detector_health, DetectorHealth::Error(_)));
        assert!(info.applied.is_none());
        assert!(!info.vpn_up);
        assert_eq!(info.routing_state, RoutingState::VpnDown);

        // The old wg0 watch was still torn down on the failed re-arm, and no
        // watch was ever recorded for tun9 (its `watch` errored first).
        let wg0 = shared
            .watches
            .lock()
            .unwrap()
            .iter()
            .find(|w| w.interface == "wg0")
            .cloned()
            .expect("wg0 watch recorded");
        wait_until(|| wg0.stopped.load(Ordering::SeqCst)).await;
        assert!(shared
            .watches
            .lock()
            .unwrap()
            .iter()
            .all(|w| w.interface != "tun9"));
    }

    #[tokio::test]
    async fn switch_where_old_revert_fails_orphans_then_cleans_up_the_old_interface() {
        // Regression for the review: if the old interface's revert fails during a
        // switch, the new interface's successful apply overwrites `applied` and
        // would forget the old one. It must instead be tracked as orphaned and
        // cleaned up once the backend recovers.
        let backend = Arc::new(MockBackend::default());
        let shared = Arc::new(MockDetectorShared::default());
        let (mut sm, mut rx) = rearm_machine(
            backend.clone(),
            shared.clone(),
            config(true, &["a.com"]),
            "rearm-orphan",
        );

        // Apply on wg0.
        sm.arm_watch();
        assert!(pump_one_command(&mut sm, &mut rx).await);
        assert_eq!(backend.applies.lock().unwrap()[0].0, "wg0");

        // Switch to wg1, but the wg0 revert fails: wg0 is orphaned, not forgotten.
        backend.set_fail_revert(true);
        let view = ConfigView {
            vpn_name: "wg1".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        let resp = sm.on_request(Request::SetConfig(view)).await;
        assert!(matches!(resp, Response::Error(_)), "old revert failed");
        assert_eq!(sm.orphaned, vec!["wg0".to_string()]);
        assert!(sm.applied.is_none(), "applied handed off to orphaned");

        // The backend recovers; the new watch's apply on wg1 then drives a
        // reconcile that also retries the orphaned wg0 cleanup.
        backend.set_fail_revert(false);
        assert!(pump_one_command(&mut sm, &mut rx).await);
        assert!(
            backend
                .applies
                .lock()
                .unwrap()
                .iter()
                .any(|(iface, _)| iface == "wg1"),
            "new interface applied"
        );
        assert!(
            sm.orphaned.is_empty(),
            "the orphaned old interface was cleaned up"
        );
        assert_eq!(
            sm.applied.as_ref().unwrap().interface,
            "wg1",
            "applied now tracks the new interface"
        );
        // wg0 was reverted twice: the failed switch attempt, then the cleanup.
        assert_eq!(
            backend
                .reverts
                .lock()
                .unwrap()
                .iter()
                .filter(|i| *i == "wg0")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn watch_stream_ending_on_its_own_marks_detector_error() {
        // Regression for the review: `watch()` returning Ok does not mean the
        // watch is alive — some detectors open their connection asynchronously
        // and the stream can close at once (e.g. NM/D-Bus absent). The forwarding
        // task reports that, and detector_health must stop showing Active.
        let backend = Arc::new(MockBackend::default());
        let shared = Arc::new(MockDetectorShared::default());
        shared.die.lock().unwrap().insert("wg0".to_string());
        let (mut sm, mut rx) = rearm_machine(
            backend.clone(),
            shared.clone(),
            config(true, &["a.com"]),
            "watch-dies",
        );

        // arm_watch optimistically set Active...
        sm.arm_watch();
        assert_eq!(sm.detector_health, DetectorHealth::Active);
        // ...then the forwarding task signals the stream ended on its own.
        assert!(
            pump_one_command(&mut sm, &mut rx).await,
            "a WatchEnded signal is expected"
        );
        let Response::Status(info) = sm.on_request(Request::Status).await else {
            panic!("expected Status");
        };
        assert!(
            matches!(info.detector_health, DetectorHealth::Error(_)),
            "a watch that ends on its own must report an unhealthy detector, got {:?}",
            info.detector_health
        );
    }
}
