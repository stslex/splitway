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

use splitway_shared::config::{self, LocalConfig, OpenVpnConfig};
use splitway_shared::ipc::{ConfigView, Request, Response, StatusInfo};
use splitway_shared::platform::{DnsBackend, PlatformError, VpnEvent, VpnInfo};

/// Routine commands funneled into the state-owner task. Shutdown is delivered
/// out-of-band (see [`run_state`]) so it can preempt a backlog of these.
pub enum StateCommand {
    /// A VPN up/down event from the detector.
    Vpn(VpnEvent),
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
    config: LocalConfig,
    config_path: PathBuf,
    vpn_up: bool,
    /// The most recent `Up` info, used to (re-)apply rules.
    last_info: Option<VpnInfo>,
    /// What is applied right now; `None` means reverted.
    applied: Option<Applied>,
    /// Set when the last apply/revert failed and left the real system state
    /// uncertain relative to `applied` (e.g. the Linux backend rolled the link
    /// back to clean on a domain-step failure, or a `revert` failed because the
    /// link had vanished). Forces the next reconcile to act even when the
    /// desired target equals the — now possibly stale — `applied` snapshot, so a
    /// post-failure "already converged" check can never skip a needed re-apply.
    needs_resync: bool,
}

impl StateMachine {
    pub fn new(backend: Arc<dyn DnsBackend>, config: LocalConfig, config_path: PathBuf) -> Self {
        Self {
            backend,
            config,
            config_path,
            vpn_up: false,
            last_info: None,
            applied: None,
            needs_resync: false,
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
    /// After a `ReloadConfig` that changes `vpn_name`, the previous event
    /// refers to the old interface, so this returns `None` and the old
    /// interface is reverted. (Auto-apply for the new interface needs a
    /// restart — the detector watch is not restarted on reload.)
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

    /// Drive the system toward [`Self::desired`], applying or reverting only
    /// when reality differs from the goal (so it is idempotent and a no-op
    /// when already converged). Returns the backend outcome so callers can
    /// surface a failure instead of silently swallowing it.
    async fn reconcile(&mut self) -> Result<(), PlatformError> {
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
        }
    }

    /// Revert active rules on shutdown so the system never stays
    /// half-configured after the daemon exits. Returns `true` if the system
    /// is left clean (revert succeeded or nothing was applied), `false` if a
    /// revert failed and rules may still be in place.
    pub async fn shutdown(&mut self) -> bool {
        if self.applied.is_none() {
            log::info!("shutdown: nothing applied, nothing to revert");
            return true;
        }
        log::info!("shutdown: reverting active rules");
        match self.revert().await {
            Ok(()) => true,
            Err(e) => {
                log::error!("shutdown: revert failed: {e}; system may be left half-configured");
                false
            }
        }
    }

    fn status(&self) -> StatusInfo {
        StatusInfo {
            enabled: self.config.enabled,
            interface: self.config.vpn_name.clone(),
            vpn_up: self.vpn_up,
            applied: self.applied.is_some(),
            domains: self.config.vpn_hosts.clone(),
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
            Ok(next) => {
                self.warn_on_restart_only_changes(&next);
                self.config = next;
                match self.reconcile().await {
                    Ok(()) => Response::Ok,
                    Err(e) => {
                        Response::Error(format!("config reloaded, but applying it failed: {e}"))
                    }
                }
            }
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
        self.warn_on_restart_only_changes(&next);
        self.commit(next).await
    }

    /// Warn when a pending config change touches fields whose effect needs a
    /// daemon restart: the interface watch and detector are set up once at
    /// startup and are not re-armed on a live change. Must be called before
    /// `self.config` is replaced, so it compares against the current config.
    fn warn_on_restart_only_changes(&self, next: &LocalConfig) {
        if next.vpn_name != self.config.vpn_name {
            log::warn!(
                "config change updated vpn_name {} -> {}; the interface change \
                 takes effect after a daemon restart",
                self.config.vpn_name,
                next.vpn_name
            );
        }
        if next.vpn_backend != self.config.vpn_backend || next.openvpn != self.config.openvpn {
            log::warn!(
                "config change updated the VPN detector settings (vpn_backend/openvpn); \
                 the detector is selected and its watcher spawned once at startup and is not \
                 restarted on a live change, so this takes effect only after a daemon restart"
            );
        }
    }

    /// Persist `next` first; only adopt it in memory if the write succeeds,
    /// then reconcile. This keeps the in-memory config and disk in lockstep.
    /// A persisted change whose re-apply fails is reported as an error so the
    /// caller is not told "ok" while DNS is out of sync.
    async fn commit(&mut self, next: LocalConfig) -> Response {
        if let Err(e) = config::save_config_to(&self.config_path, &next) {
            return Response::Error(format!("failed to persist config: {e}"));
        }
        self.config = next;
        match self.reconcile().await {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(format!("config saved, but applying it failed: {e}")),
        }
    }
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
                    Some(StateCommand::Vpn(event)) => machine.on_event(event).await,
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

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
        StateMachine::new(backend, cfg, temp_config_path(tag))
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
        // The old interface's rules are reverted; nothing is applied to the new
        // interface (its watch is not started until a restart).
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
        assert!(info.applied);
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
}
