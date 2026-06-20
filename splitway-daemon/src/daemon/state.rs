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

use splitway_shared::config::{self, ConfigParseError, LocalConfig, OpenVpnConfig};
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

/// Config persistence behind a trait, so the [`StateMachine`]'s config handling
/// is unit-testable: a fake store can simulate a malformed file, a load error,
/// or a concurrent external edit landing between an RMW read and write — none of
/// which a real temp file exercises cleanly. Mirrors the [`DetectorFactory`]
/// injection. The file is the single source of truth; this store is the only
/// path the actor reads or writes it through (no inline `fs` access here).
pub trait ConfigStore: Send + Sync {
    /// Read and parse the config from its backing store.
    fn load(&self) -> Result<LocalConfig, ConfigParseError>;
    /// Persist the config. The production impl writes atomically (temp file then
    /// rename — see [`config::save_config_to`]).
    fn save(&self, config: &LocalConfig) -> Result<(), ConfigParseError>;
    /// A human-readable description of where the config lives (the file path for
    /// the production store), surfaced as the informational
    /// [`ConfigView::config_path`]. Never used for I/O.
    fn describe(&self) -> String;
}

/// Production store: the real config file at the path fixed at launch. `load` /
/// `save` delegate to [`config::load_config_from`] / [`config::save_config_to`];
/// the latter already performs the atomic temp-file-plus-rename write.
pub struct FileConfigStore {
    path: PathBuf,
}

impl FileConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ConfigStore for FileConfigStore {
    fn load(&self) -> Result<LocalConfig, ConfigParseError> {
        config::load_config_from(&self.path)
    }

    fn save(&self, config: &LocalConfig) -> Result<(), ConfigParseError> {
        config::save_config_to(&self.path, config)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
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
    /// The config file on disk changed — the file watcher saw an event touching
    /// it (an external hand-edit). Carries no data: the file is the single source
    /// of truth, so the actor always re-reads it. The actor's equality check
    /// debounces the daemon's own writes and coalesces a burst into one reload
    /// (see [`StateMachine::on_config_changed`]).
    ConfigChanged,
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
    /// An in-memory working copy of the config the file watcher keeps reconciled
    /// to disk — the file is the single source of truth, so this is not a
    /// free-floating cache. It backs the cheap, infallible reads on the `status()`
    /// hot path and is the re-arm baseline (`watch_settings_changed`); every
    /// mutation is read-modify-write from disk through [`Self::config_store`].
    config: LocalConfig,
    /// The only path the actor reads or writes the config through. Injected for
    /// testability (see [`ConfigStore`]).
    config_store: Arc<dyn ConfigStore>,
    /// Set when the last load (an RMW read or a watcher reload) failed to parse,
    /// so the daemon froze on the last-good `config`. Drives the
    /// highest-precedence [`RoutingState::ConfigInvalid`]; cleared on the next
    /// successful load (see [`Self::load_fresh`]).
    config_invalid: bool,
    vpn_up: bool,
    /// The most recent `Up` info, used to (re-)apply rules.
    last_info: Option<VpnInfo>,
    /// What is applied right now; `None` means reverted.
    applied: Option<Applied>,
    /// Interfaces whose rules a live switch could not revert, and which are no
    /// longer the configured interface — so the new interface's apply (which
    /// overwrites `applied`) would otherwise forget them. A later reconcile or
    /// shutdown keeps retrying their cleanup. Almost always empty. Only populated
    /// for per-interface-revert backends: a global-revert backend (macOS) never
    /// orphans, because its revert would also wipe the active interface (see
    /// [`Self::adopt_config`] and [`DnsBackend::reverts_globally`]).
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
        config_store: Arc<dyn ConfigStore>,
        state_tx: mpsc::Sender<StateCommand>,
    ) -> Self {
        Self {
            backend,
            detector_factory,
            state_tx,
            config,
            config_store,
            config_invalid: false,
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
    ///   notices the dropped receiver only inside its change callback, i.e. on
    ///   the next network/DNS change, and then stops; until then it sits parked.
    ///   A config-driven re-arm (e.g. editing `vpn_name` in the GUI) need not
    ///   coincide with any network change, so on a quiet network each such re-arm
    ///   leaves the previous thread parked until the next network/DNS event or
    ///   process exit — i.e. transiently more than one can park. They hold no live
    ///   resources and deliver no events (the receiver is gone), and all are
    ///   reaped at process exit, so this is a bounded, self-healing backlog rather
    ///   than a growing leak. Deterministic teardown (stopping the run loop from
    ///   the actor on re-arm) is a macOS follow-up — see `detector/macos/watch.rs`.
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

    /// Drive the system toward [`Self::desired`], then retry cleanup of any
    /// interface orphaned by an earlier failed switch-revert (almost always a
    /// no-op — the list is empty).
    ///
    /// The primary reconcile is the caller's main action, so its failure takes
    /// precedence. But when the primary succeeds while a known orphan still
    /// carries stale rules, surface *that* failure rather than a clean `Ok`:
    /// otherwise Disable/Resync/etc. would report success while split-DNS still
    /// lingers on the orphaned link until shutdown or a later retry.
    async fn reconcile(&mut self) -> Result<(), PlatformError> {
        let primary = self.reconcile_primary().await;
        let orphan = self.revert_orphaned().await;
        primary.and(orphan)
    }

    /// Best-effort cleanup of interfaces orphaned by a failed revert during a
    /// live switch (see [`Self::adopt_config`]): the new interface's successful
    /// apply overwrites `applied`, so a stale old interface is tracked in
    /// `orphaned` instead and reverted here whenever the backend recovers.
    /// Successes drop from the list; failures stay for the next attempt. The
    /// currently-applied interface is never reverted here — a switch back to a
    /// still-orphaned interface re-applies its rules, and `applied` (not this
    /// list) then owns them.
    ///
    /// Returns `Err` if any interface still needs cleanup after this attempt, so
    /// [`Self::reconcile`] can surface the lingering half-configured state to
    /// callers instead of masking it behind a successful primary reconcile.
    async fn revert_orphaned(&mut self) -> Result<(), PlatformError> {
        // The currently-applied interface is owned by `applied`, never orphaned:
        // if the user switched back to an interface still awaiting orphan cleanup
        // and a fresh `Up` just re-applied its rules in `reconcile_primary`,
        // reverting it here would tear those live rules down while `applied` still
        // reports them installed. Drop it from the list — `applied` owns its
        // lifecycle now (a later down/disable/switch reverts it through `revert`).
        if let Some(applied) = &self.applied {
            self.orphaned
                .retain(|interface| interface != &applied.interface);
        }
        if self.orphaned.is_empty() {
            return Ok(());
        }
        let mut last_err: Option<PlatformError> = None;
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
                    last_err = Some(e);
                }
                Err(e) => {
                    log::error!("orphan revert task panicked for {interface}: {e}");
                    last_err = Some(PlatformError::CommandFailed(format!(
                        "orphan revert task panicked for {interface}: {e}"
                    )));
                    self.orphaned.push(interface);
                }
            }
        }
        if self.orphaned.is_empty() {
            Ok(())
        } else {
            // Name the lingering interface(s) so the surfaced message is not
            // mistaken for the caller's primary action failing — these stale
            // rules are a leftover from an earlier failed switch.
            Err(PlatformError::CommandFailed(format!(
                "stale split-DNS rules remain on {} (orphaned by an earlier failed switch) \
                 and could not be cleaned up{}",
                self.orphaned.join(", "),
                last_err.map(|e| format!(": {e}")).unwrap_or_default()
            )))
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
            Request::ListInterfaces => {
                // Enumeration is blocking platform I/O (reads `/sys/class/net` on
                // Linux, `getifaddrs` on macOS) and the GUI re-polls it on every
                // refresh, so run it on the blocking pool — like apply/revert —
                // rather than on the actor task, where it would stall VPN-event
                // and IPC handling while the syscalls run.
                match tokio::task::spawn_blocking(list_interfaces).await {
                    Ok(Ok(interfaces)) => Response::Interfaces(interfaces),
                    // Enumeration failure is a clean error to the client, never a
                    // panic — the GUI falls back to free-text entry.
                    Ok(Err(e)) => Response::Error(format!("failed to list interfaces: {e}")),
                    Err(e) => Response::Error(format!("interface enumeration task panicked: {e}")),
                }
            }
        }
    }

    /// Revert active rules on shutdown so the system never stays
    /// half-configured after the daemon exits — both the currently-applied
    /// interface and any orphaned by a failed live switch. Returns `true` if the
    /// system is left clean, `false` if a revert failed and rules may remain.
    pub async fn shutdown(&mut self) -> bool {
        // Retry any orphaned-interface cleanup first, so a switch whose old
        // revert failed does not strand rules past shutdown. Its Result is
        // intentionally ignored: shutdown reports cleanliness from the
        // `self.orphaned` check below, which covers the same lingering set.
        let _ = self.revert_orphaned().await;
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
    ///
    /// A known out-of-sync condition takes precedence over every "inactive"
    /// reason: a failed apply/revert (`needs_resync`) or an interface orphaned by
    /// a failed switch (`orphaned` non-empty) means stale split-DNS rules may
    /// still be installed somewhere, so reporting `Disabled`/`Applied`/etc. would
    /// claim a clean state the daemon does not believe in. Both surface as
    /// `ApplyFailed` ("out of sync") until cleanup succeeds — e.g. a `Disable`
    /// whose revert failed reads `ApplyFailed`, not `Disabled`.
    fn routing_state(&self) -> RoutingState {
        // A config file that does not parse takes precedence over everything:
        // the daemon froze on the last-good config, and the user must learn
        // their edit was rejected rather than see a stale "applied" (see
        // `on_config_changed`). Clears automatically on the next valid load.
        if self.config_invalid {
            return RoutingState::ConfigInvalid;
        }
        // Out-of-sync (a failed apply/revert, or a lingering orphaned interface)
        // overrides the inactive-reason branches below — see the doc above.
        if self.needs_resync || !self.orphaned.is_empty() {
            return RoutingState::ApplyFailed;
        }
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

    /// Load the config through the store, maintaining the `config_invalid` freeze
    /// flag: cleared on a successful parse, set on any parse/read failure. Every
    /// disk read of the config — the RMW mutations below, the manual reload, and
    /// the watcher reload — goes through here, so the flag (and thus
    /// [`RoutingState::ConfigInvalid`]) always reflects the latest load attempt.
    fn load_fresh(&mut self) -> Result<LocalConfig, ConfigParseError> {
        match self.config_store.load() {
            Ok(config) => {
                self.config_invalid = false;
                Ok(config)
            }
            Err(e) => {
                self.config_invalid = true;
                Err(e)
            }
        }
    }

    async fn set_enabled(&mut self, enabled: bool) -> Response {
        // Read-modify-write from disk: load the current file, apply only this
        // verb's delta, then persist + adopt — so a concurrent external edit to
        // other fields is merged, never clobbered from a stale snapshot.
        let mut next = match self.load_fresh() {
            Ok(config) => config,
            Err(e) => return config_unreadable_reply(e),
        };
        // The "no change" early-out is evaluated against the freshly-loaded
        // value, not a possibly-stale `self.config`.
        if next.enabled == enabled {
            // Nothing to persist, but adopt the loaded config (it may carry an
            // external edit) and reconcile so a previous failed apply/revert
            // retries instead of reporting success while still out of sync.
            return match self.adopt_config(next).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(format!("failed to apply current state: {e}")),
            };
        }
        next.enabled = enabled;
        self.commit(next).await
    }

    /// Note the `already present` error path is **not inert**: it first adopts +
    /// reconciles the freshly-loaded config (so a concurrent external edit
    /// converges without relying on the watcher), and may therefore re-arm the
    /// watch or change applied rules before returning the error. The error is
    /// preserved for the normal duplicate-add contract; a caller must not read it
    /// as "nothing happened".
    async fn add_domain(&mut self, domain: String) -> Response {
        let mut next = match self.load_fresh() {
            Ok(config) => config,
            Err(e) => return config_unreadable_reply(e),
        };
        if next.vpn_hosts.iter().any(|d| d == &domain) {
            // Already present on disk: nothing to add. Still adopt the
            // freshly-loaded config (it may carry a concurrent external edit) and
            // reconcile, so the daemon converges to the source-of-truth file even
            // if the watcher has not (or cannot) deliver that edit — then report
            // the no-op error. A failed reconcile is surfaced over the success.
            if let Err(e) = self.adopt_config(next).await {
                return Response::Error(format!(
                    "domain already present: {domain} (and applying the current config failed: {e})"
                ));
            }
            return Response::Error(format!("domain already present: {domain}"));
        }
        next.vpn_hosts.push(domain);
        self.commit(next).await
    }

    async fn remove_domain(&mut self, domain: String) -> Response {
        let mut next = match self.load_fresh() {
            Ok(config) => config,
            Err(e) => return config_unreadable_reply(e),
        };
        if !next.vpn_hosts.iter().any(|d| d == &domain) {
            // Absent on disk: nothing to remove. Adopt the freshly-loaded config
            // and reconcile anyway (mirroring the no-change `set_enabled` path):
            // an external edit may already have removed the domain, and its rules
            // must be reverted now rather than waiting on the best-effort watcher
            // — otherwise this would report success while DNS stays out of sync.
            return match self.adopt_config(next).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(format!("failed to apply current state: {e}")),
            };
        }
        next.vpn_hosts.retain(|d| d != &domain);
        self.commit(next).await
    }

    async fn reload_config(&mut self) -> Response {
        match self.load_fresh() {
            Ok(next) => match self.adopt_config(next).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(format!("config reloaded, but applying it failed: {e}")),
            },
            Err(e) => Response::Error(format!("failed to reload config: {e}")),
        }
    }

    /// Handle a [`StateCommand::ConfigChanged`] from the file watcher: re-read the
    /// file (the single source of truth) and reconcile to it. The equality check
    /// debounces the daemon's own writes — after an RMW save `self.config` already
    /// equals disk, so the watcher event for that write is a no-op — and
    /// coalesces a burst of events for one save into a single reload. A parse
    /// failure freezes on the last-good config and surfaces
    /// [`RoutingState::ConfigInvalid`] (set via [`Self::load_fresh`]); recovery is
    /// automatic on the next valid edit.
    ///
    /// A non-atomic hand-edit (an editor that truncates-then-writes in place) can
    /// be observed mid-write and briefly read as invalid, flipping to
    /// `ConfigInvalid` until the completing write fires another event — which it
    /// normally does (a coalesced event re-reads the *latest*, now-valid, state).
    /// A permanent latch would require the watcher to drop the trailing event
    /// entirely (inotify queue overflow), which is rare and not config-specific.
    /// Hand-editing atomically (write a temp file and rename over the config, as
    /// the daemon's own writes do) avoids the transient window; this is noted in
    /// `docs/architecture.md`.
    async fn on_config_changed(&mut self) {
        match self.load_fresh() {
            Ok(loaded) => {
                if loaded == self.config {
                    return;
                }
                log::info!("config file changed on disk; reloading");
                if let Err(e) = self.adopt_config(loaded).await {
                    log::error!("applying externally-edited config failed: {e}");
                }
            }
            Err(e) => {
                log::warn!("config file on disk is invalid ({e}); keeping the last-good config");
            }
        }
    }

    /// Build the editable config projection sent in reply to
    /// [`Request::GetConfig`]. The `config_path` is the daemon's effective config
    /// location (from the store), informational only — [`Self::set_config`]
    /// ignores it.
    fn config_view(&self) -> ConfigView {
        ConfigView {
            vpn_name: self.config.vpn_name.clone(),
            vpn_backend: self.config.vpn_backend,
            openvpn_management: self.config.openvpn.management.clone(),
            openvpn_management_password_file: self.config.openvpn.management_password_file.clone(),
            config_path: self.config_store.describe(),
        }
    }

    /// Apply a [`Request::SetConfig`] update. Overwrites only the editable
    /// projection's fields (`vpn_name`, `vpn_backend`, `openvpn.*`), preserving
    /// `enabled` and the domain list owned by the other verbs, then persists
    /// and reconciles through the single-writer [`Self::commit`] path. The
    /// incoming `config_path` is ignored: the active path is fixed at launch.
    async fn set_config(&mut self, view: ConfigView) -> Response {
        // Read-modify-write from disk: overwrite only the editable projection on
        // the *loaded* config, so a concurrent external edit to `enabled` or the
        // domain list (owned by the other verbs) is preserved, not clobbered.
        let mut next = match self.load_fresh() {
            Ok(config) => config,
            Err(e) => return config_unreadable_reply(e),
        };
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
        // Residual: the RMW load and this save are not atomic w.r.t. an external
        // writer (a narrow TOCTOU window). Acceptable here — the actor is the only
        // daemon writer and hand-edits are manual/rare.
        // TODO(phase-8): take an flock around the load→save pair to close it.
        if let Err(e) = self.config_store.save(&next) {
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
            // (successful) apply would overwrite `applied` and forget it. On a
            // per-interface backend (Linux/resolvectl) we hand it to the orphaned
            // list, which a later reconcile or shutdown retries — otherwise a
            // switch where old cleanup fails but new apply succeeds would strand
            // the old interface's split-DNS rules.
            //
            // On a GLOBAL-revert backend (macOS removes every managed resolver
            // file regardless of interface) we must NOT track it: the orphan
            // cleanup's `revert_rules` would also wipe the freshly-applied
            // interface's rules while `applied` still records them. There the new
            // apply overwrites the same shared (per-domain) state and any future
            // revert is global, so leaving `applied`/`needs_resync` as the
            // failed-revert snapshot is correct and self-healing.
            let stale = match &self.applied {
                Some(applied)
                    if applied.interface != self.config.vpn_name
                        && !self.backend.reverts_globally() =>
                {
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

/// The reply when a mutation cannot read the config file (missing or malformed).
/// Mutations are read-modify-write and deliberately refuse to write a config
/// derived from one they could not read — `set_config` preserves the file's
/// `enabled`/`vpn_hosts`, so overwriting an unreadable file would clobber them.
/// The file must therefore be fixed *on disk*; the daemon keeps running on the
/// last-good config meanwhile (surfaced as [`RoutingState::ConfigInvalid`]). The
/// message guides the user/GUI to that recovery, since no IPC verb can repair a
/// file the daemon cannot parse.
fn config_unreadable_reply(e: ConfigParseError) -> Response {
    Response::Error(format!(
        "cannot change settings: the config file on disk could not be read ({e}) — \
         fix it on disk; the daemon keeps running on the last-good config"
    ))
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
                    Some(StateCommand::ConfigChanged) => machine.on_config_changed().await,
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
        global_revert: AtomicBool,
    }

    impl MockBackend {
        fn set_fail_apply(&self, fail: bool) {
            self.fail_apply.store(fail, Ordering::Relaxed);
        }

        fn set_fail_revert(&self, fail: bool) {
            self.fail_revert.store(fail, Ordering::Relaxed);
        }

        fn set_reverts_globally(&self, global: bool) {
            self.global_revert.store(global, Ordering::Relaxed);
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

        fn reverts_globally(&self) -> bool {
            self.global_revert.load(Ordering::Relaxed)
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

    /// A real file-backed [`ConfigStore`] over a temp path, pre-seeded with `cfg`
    /// so the first RMW load succeeds — mirroring production, where
    /// `load_or_init_config` writes the file before the actor starts.
    fn file_store(tag: &str, cfg: &LocalConfig) -> Arc<dyn ConfigStore> {
        let store = FileConfigStore::new(temp_config_path(tag));
        store.save(cfg).unwrap();
        Arc::new(store)
    }

    /// An in-memory [`ConfigStore`] for tests that need to simulate what a real
    /// temp file cannot exercise cleanly: a malformed/unreadable file (`None`
    /// "on disk"), a concurrent external edit (`set_external`), and a count of
    /// the actor's own writes (`save_count`, for the self-write debounce check).
    #[derive(Clone)]
    struct FakeConfigStore {
        inner: Arc<Mutex<FakeStoreState>>,
    }

    struct FakeStoreState {
        /// `Some` = a valid config "on disk"; `None` = the file does not parse.
        current: Option<LocalConfig>,
        saves: usize,
    }

    impl FakeConfigStore {
        fn new(cfg: LocalConfig) -> Self {
            FakeConfigStore {
                inner: Arc::new(Mutex::new(FakeStoreState {
                    current: Some(cfg),
                    saves: 0,
                })),
            }
        }

        /// Simulate an external hand-edit landing a new valid config on disk.
        fn set_external(&self, cfg: LocalConfig) {
            self.inner.lock().unwrap().current = Some(cfg);
        }

        /// Simulate a malformed/unreadable file (a load will fail to parse).
        fn set_malformed(&self) {
            self.inner.lock().unwrap().current = None;
        }

        /// The current config "on disk", or `None` when malformed.
        fn current(&self) -> Option<LocalConfig> {
            self.inner.lock().unwrap().current.clone()
        }

        /// How many times the actor persisted through this store.
        fn save_count(&self) -> usize {
            self.inner.lock().unwrap().saves
        }
    }

    impl ConfigStore for FakeConfigStore {
        fn load(&self) -> Result<LocalConfig, ConfigParseError> {
            match &self.inner.lock().unwrap().current {
                Some(cfg) => Ok(cfg.clone()),
                None => Err(ConfigParseError::SerializeError),
            }
        }

        fn save(&self, config: &LocalConfig) -> Result<(), ConfigParseError> {
            let mut state = self.inner.lock().unwrap();
            state.current = Some(config.clone());
            state.saves += 1;
            Ok(())
        }

        fn describe(&self) -> String {
            "<in-memory test store>".to_string()
        }
    }

    /// Build a machine over an explicit store (used by the fake-store tests).
    fn machine_with_store(
        backend: Arc<MockBackend>,
        store: Arc<dyn ConfigStore>,
        cfg: LocalConfig,
    ) -> StateMachine {
        let (state_tx, _state_rx) = mpsc::channel(16);
        StateMachine::new(backend, Arc::new(NoopDetectorFactory), cfg, store, state_tx)
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
        let store = file_store(tag, &cfg);
        StateMachine::new(backend, Arc::new(NoopDetectorFactory), cfg, store, state_tx)
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
        let store = file_store(tag, &cfg);
        let sm = StateMachine::new(backend, factory, cfg, store, state_tx);
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
        let saved = sm.config_store.load().unwrap();
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
        let saved = sm.config_store.load().unwrap();
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
        sm.config_store.save(&new_cfg).unwrap();
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
        let saved = sm.config_store.load().unwrap();
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
        assert_eq!(view.config_path, sm.config_store.describe());
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
        let saved = sm.config_store.load().unwrap();
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

        // Disabled, but the disable's revert failed (applied still set,
        // needs_resync): stale DNS may linger, so this must read ApplyFailed
        // rather than a clean Disabled.
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "rs-disable-fail");
        sm.on_event(vpn_up("wg0")).await;
        backend.set_fail_revert(true);
        let _ = sm.on_request(Request::Disable).await;
        assert!(!sm.config.enabled);
        assert!(sm.applied.is_some() && sm.needs_resync);
        assert_eq!(sm.routing_state(), RoutingState::ApplyFailed);

        // A switch whose old-interface revert failed leaves it orphaned while the
        // new interface applies cleanly (applied set, needs_resync false): stale
        // rules linger on the old link, so this must read ApplyFailed rather than
        // a clean Applied.
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "rs-orphan");
        sm.on_event(vpn_up("wg0")).await;
        backend.set_fail_revert(true);
        let to_wg1 = ConfigView {
            vpn_name: "wg1".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        let _ = sm.on_request(Request::SetConfig(to_wg1)).await;
        sm.on_event(vpn_up("wg1")).await;
        assert_eq!(sm.applied.as_ref().unwrap().interface, "wg1");
        assert_eq!(sm.orphaned, vec!["wg0".to_string()]);
        assert!(!sm.needs_resync);
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
    async fn switching_back_to_an_orphaned_interface_does_not_revert_freshly_applied_rules() {
        // Regression for the review: an interface can be orphaned (its
        // switch-away revert failed) and then become the active interface again
        // (the user switches back) before its cleanup succeeds. Once a fresh `Up`
        // re-applies its rules, the opportunistic orphan cleanup must NOT revert
        // them — that would strip live DNS while `applied` still reports them
        // installed.
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(
            backend.clone(),
            config(true, &["a.com"]),
            "orphan-switchback",
        );

        // Apply on wg0.
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(sm.applied.as_ref().unwrap().interface, "wg0");

        // Switch to wg1 with reverts failing: wg0's revert fails, so it is
        // orphaned (not forgotten) and `applied` is handed off to the new target.
        backend.set_fail_revert(true);
        let to_wg1 = ConfigView {
            vpn_name: "wg1".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        assert!(matches!(
            sm.on_request(Request::SetConfig(to_wg1)).await,
            Response::Error(_)
        ));
        assert_eq!(sm.orphaned, vec!["wg0".to_string()]);
        assert!(sm.applied.is_none());

        // Switch back to wg0 while cleanup is still failing: wg0 stays orphaned
        // and is once again the configured interface. The switch's primary
        // reconcile is a clean no-op (nothing is applied yet), but the orphan
        // revert still fails, so the reply surfaces that lingering cleanup rather
        // than a bare Ok (see `reconcile`).
        let to_wg0 = ConfigView {
            vpn_name: "wg0".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        match sm.on_request(Request::SetConfig(to_wg0)).await {
            Response::Error(msg) => assert!(
                msg.contains("wg0"),
                "the error should name the still-orphaned interface, got: {msg}"
            ),
            other => panic!("expected Error surfacing the orphan cleanup failure, got {other:?}"),
        }
        assert_eq!(sm.orphaned, vec!["wg0".to_string()]);

        // The backend recovers and wg0 comes back up: the fresh apply owns wg0,
        // and orphan cleanup must skip it rather than revert what was just applied.
        backend.set_fail_revert(false);
        sm.on_event(vpn_up("wg0")).await;

        assert_eq!(
            sm.applied.as_ref().unwrap().interface,
            "wg0",
            "the re-applied interface is tracked as applied"
        );
        assert!(
            sm.orphaned.is_empty(),
            "wg0 is no longer orphaned — `applied` owns it now"
        );
        // wg0 was reverted exactly twice (the failed switch-away, then the failed
        // orphan retry on switch-back) and NOT a third time after the re-apply.
        assert_eq!(
            backend
                .reverts
                .lock()
                .unwrap()
                .iter()
                .filter(|i| *i == "wg0")
                .count(),
            2,
            "orphan cleanup must not revert the freshly re-applied interface"
        );
        // The final re-apply on wg0 did happen.
        assert!(backend
            .applies
            .lock()
            .unwrap()
            .iter()
            .any(|(iface, _)| iface == "wg0"));
    }

    #[tokio::test]
    async fn disable_surfaces_an_orphan_cleanup_failure_instead_of_reporting_ok() {
        // Regression for the review: when an interface is orphaned by a failed
        // switch, a later action whose own primary reconcile succeeds must not
        // report Ok while the orphan's stale rules still cannot be cleaned. Here
        // Disable's primary reconcile is a clean no-op (nothing is applied), yet
        // the orphan revert keeps failing — `reconcile` folds that failure into
        // its result so the caller is not told the system is clean.
        let backend = Arc::new(MockBackend::default());
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "orphan-disable");

        // Apply on wg0.
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(sm.applied.as_ref().unwrap().interface, "wg0");

        // Switch to wg1 with reverts failing: wg0 is orphaned, applied handed off.
        backend.set_fail_revert(true);
        let to_wg1 = ConfigView {
            vpn_name: "wg1".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        assert!(matches!(
            sm.on_request(Request::SetConfig(to_wg1)).await,
            Response::Error(_)
        ));
        assert_eq!(sm.orphaned, vec!["wg0".to_string()]);
        assert!(sm.applied.is_none());

        // Disable: the primary reconcile reverts nothing (applied is None) and
        // succeeds, but the orphaned wg0 still cannot be reverted — so the reply
        // must be an Error that names wg0, not Ok.
        match sm.on_request(Request::Disable).await {
            Response::Error(msg) => assert!(
                msg.contains("wg0"),
                "Disable should surface the orphan cleanup failure, got: {msg}"
            ),
            other => panic!("expected Error surfacing the orphan cleanup failure, got {other:?}"),
        }
        assert_eq!(
            sm.orphaned,
            vec!["wg0".to_string()],
            "the orphan stays tracked for a later retry"
        );

        // Once the backend recovers, the next reconcile (here via re-enable)
        // cleans the orphan and the operation reports Ok again.
        backend.set_fail_revert(false);
        assert_eq!(sm.on_request(Request::Enable).await, Response::Ok);
        assert!(
            sm.orphaned.is_empty(),
            "the orphan is cleaned once the backend recovers"
        );
    }

    #[tokio::test]
    async fn global_revert_backend_does_not_track_orphans_on_a_failed_switch() {
        // Regression for the review: on a backend whose revert is global (macOS
        // removes every managed resolver file regardless of interface), tracking a
        // single orphaned interface is unsafe — the orphan cleanup's revert would
        // also wipe the freshly-applied interface's rules while `applied` still
        // records them. Such a backend must not orphan; the new apply overwrites
        // the shared state instead.
        let backend = Arc::new(MockBackend::default());
        backend.set_reverts_globally(true);
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "global-revert");

        // Apply on wg0.
        sm.on_event(vpn_up("wg0")).await;
        assert_eq!(sm.applied.as_ref().unwrap().interface, "wg0");

        // Switch to wg1 with reverts failing: a per-interface backend would orphan
        // wg0, but a global-revert backend must not.
        backend.set_fail_revert(true);
        let to_wg1 = ConfigView {
            vpn_name: "wg1".to_string(),
            vpn_backend: config::VpnBackend::default(),
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        };
        let _ = sm.on_request(Request::SetConfig(to_wg1)).await;
        assert!(
            sm.orphaned.is_empty(),
            "a global-revert backend must not track an orphaned interface"
        );

        // wg1 comes up and applies cleanly. Because nothing was orphaned, orphan
        // cleanup never runs a (global) revert that would wipe wg1's fresh rules.
        backend.set_fail_revert(false);
        sm.on_event(vpn_up("wg1")).await;
        assert_eq!(sm.applied.as_ref().unwrap().interface, "wg1");
        assert!(sm.orphaned.is_empty());
        assert!(
            !backend.reverts.lock().unwrap().iter().any(|i| i == "wg1"),
            "the freshly-applied interface must never be reverted by orphan cleanup"
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

    // ---- Phase 5c: config as the single source of truth ----

    #[tokio::test]
    async fn mutation_is_read_modify_write_merging_a_concurrent_external_edit() {
        // The in-memory config starts as {wg0, [a.com]}; a concurrent external
        // edit changes a *different* field (vpn_name -> tun9). add_domain must
        // load fresh and merge, so the saved config keeps the external vpn_name
        // AND gains the new domain — not clobber tun9 from the stale snapshot.
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend, store, config(true, &["a.com"]));

        let mut external = config(true, &["a.com"]);
        external.vpn_name = "tun9".to_string();
        fake.set_external(external);

        assert_eq!(sm.add_domain("b.com".to_string()).await, Response::Ok);

        let saved = fake.current().expect("config still valid");
        assert_eq!(
            saved.vpn_name, "tun9",
            "external vpn_name edit was clobbered"
        );
        assert_eq!(saved.vpn_hosts, vec!["a.com", "b.com"]);
        // The in-memory working copy is kept in lockstep with disk.
        assert_eq!(sm.config.vpn_name, "tun9");
        assert_eq!(sm.config.vpn_hosts, vec!["a.com", "b.com"]);
    }

    #[tokio::test]
    async fn mutation_with_unreadable_config_errors_without_writing_and_freezes() {
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend, store, config(true, &["a.com"]));

        // The file becomes malformed before the RMW read.
        fake.set_malformed();
        let response = sm.add_domain("b.com".to_string()).await;

        assert!(
            matches!(response, Response::Error(_)),
            "an RMW load failure must error"
        );
        assert_eq!(
            fake.save_count(),
            0,
            "no config may be written after a failed read"
        );
        // The in-memory config is untouched, and the failure is surfaced.
        assert_eq!(sm.config.vpn_hosts, vec!["a.com"]);
        assert_eq!(sm.routing_state(), RoutingState::ConfigInvalid);
    }

    #[tokio::test]
    async fn self_write_does_not_trigger_a_redundant_reconcile() {
        // After the daemon's own write, self.config already equals disk, so the
        // watcher event for that write must be a no-op (the equality skip) and
        // must not re-run apply.
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend.clone(), store, config(true, &["a.com"]));

        sm.on_event(vpn_up("wg0")).await;
        let applies_before = backend.applies.lock().unwrap().len();
        assert_eq!(applies_before, 1);

        // Simulate the watcher firing for the daemon's own (equal) write.
        sm.on_config_changed().await;

        assert_eq!(
            backend.applies.lock().unwrap().len(),
            applies_before,
            "an equal config must not re-apply"
        );
    }

    #[tokio::test]
    async fn external_edit_to_a_watch_field_reloads_and_rearms() {
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend, store, config(true, &["a.com"]));

        // Arm once so the generation has a baseline.
        sm.arm_watch();
        let generation_before = sm.watch_generation;

        let mut external = config(true, &["a.com"]);
        external.vpn_name = "tun9".to_string();
        fake.set_external(external);

        sm.on_config_changed().await;

        assert_eq!(
            sm.config.vpn_name, "tun9",
            "external watch-field edit was not adopted"
        );
        assert!(
            sm.watch_generation > generation_before,
            "a watch-field change must re-arm the watch"
        );
    }

    #[tokio::test]
    async fn external_edit_to_a_non_watch_field_reloads_without_rearm() {
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend, store, config(true, &["a.com"]));

        sm.arm_watch();
        let generation_before = sm.watch_generation;

        // Only the domain list changes (not a watch field).
        fake.set_external(config(true, &["a.com", "c.com"]));
        sm.on_config_changed().await;

        assert_eq!(
            sm.config.vpn_hosts,
            vec!["a.com", "c.com"],
            "external domain edit not adopted"
        );
        assert_eq!(
            sm.watch_generation, generation_before,
            "a domain-only change must not re-arm the watch"
        );
    }

    #[tokio::test]
    async fn malformed_file_freezes_on_last_good_then_recovers() {
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend.clone(), store, config(true, &["a.com"]));

        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());
        let applies_after_first = backend.applies.lock().unwrap().len();

        // A malformed hand-edit: freeze on the last-good config, surface invalid.
        fake.set_malformed();
        sm.on_config_changed().await;
        assert_eq!(sm.routing_state(), RoutingState::ConfigInvalid);
        assert_eq!(
            sm.config.vpn_hosts,
            vec!["a.com"],
            "the last-good config must be kept"
        );
        assert!(
            sm.applied.is_some(),
            "applied rules must be held while frozen"
        );
        assert_eq!(
            backend.applies.lock().unwrap().len(),
            applies_after_first,
            "a malformed file must not re-apply or revert"
        );

        // The user fixes the file: recovery is automatic on the next load.
        fake.set_external(config(true, &["a.com", "c.com"]));
        sm.on_config_changed().await;
        assert!(
            !sm.config_invalid,
            "a valid file must clear the freeze flag"
        );
        assert_ne!(sm.routing_state(), RoutingState::ConfigInvalid);
        assert_eq!(sm.config.vpn_hosts, vec!["a.com", "c.com"]);
    }

    #[tokio::test]
    async fn remove_of_an_externally_removed_domain_adopts_disk_and_reverts() {
        // The watcher's event has not arrived (or it is unavailable): an external
        // edit already removed the only domain on disk while the daemon still has
        // it applied. Removing that now-absent domain must adopt the disk config
        // and revert the stale rules — not report success while DNS stays out of
        // sync (the no-op early-out must reconcile, not just return Ok).
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend.clone(), store, config(true, &["a.com"]));

        sm.on_event(vpn_up("wg0")).await;
        assert!(sm.applied.is_some());

        // External edit removes the domain on disk before the IPC remove runs.
        fake.set_external(config(true, &[]));
        let resp = sm.remove_domain("a.com".to_string()).await;

        assert_eq!(resp, Response::Ok);
        assert!(
            sm.config.vpn_hosts.is_empty(),
            "the disk removal must be adopted"
        );
        assert!(
            sm.applied.is_none(),
            "the removed domain's rules must be reverted, not left applied"
        );
        assert_eq!(sm.routing_state(), RoutingState::NoDomains);
    }

    #[tokio::test]
    async fn recovery_to_an_identical_last_good_config_clears_invalid() {
        // Recovery must work even when the user reverts a malformed edit back to a
        // file *identical* to the last-good config: `load_fresh` clears
        // `config_invalid` before the equality check short-circuits the reload, so
        // the freeze lifts without a redundant reconcile.
        let backend = Arc::new(MockBackend::default());
        let fake = FakeConfigStore::new(config(true, &["a.com"]));
        let store: Arc<dyn ConfigStore> = Arc::new(fake.clone());
        let mut sm = machine_with_store(backend.clone(), store, config(true, &["a.com"]));

        sm.on_event(vpn_up("wg0")).await;
        let applies_after_first = backend.applies.lock().unwrap().len();

        // A malformed edit freezes on the last-good config.
        fake.set_malformed();
        sm.on_config_changed().await;
        assert!(sm.config_invalid);
        assert_eq!(sm.routing_state(), RoutingState::ConfigInvalid);

        // The user reverts to a file identical to the last-good config.
        fake.set_external(config(true, &["a.com"]));
        sm.on_config_changed().await;

        assert!(
            !sm.config_invalid,
            "an identical valid file must clear the freeze"
        );
        assert_eq!(sm.routing_state(), RoutingState::Applied);
        assert_eq!(
            backend.applies.lock().unwrap().len(),
            applies_after_first,
            "recovery to an identical config must not re-apply (equality skip)"
        );
    }
}
