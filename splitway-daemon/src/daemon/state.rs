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

use splitway_shared::config::{self, LocalConfig};
use splitway_shared::ipc::{Request, Response, StatusInfo};
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

/// A snapshot of what is currently applied to the system.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    interface: String,
    domains: Vec<String>,
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
        }
    }

    /// What *should* be applied given the current config and VPN state.
    /// `None` means "nothing should be applied" (revert to direct DNS).
    ///
    /// An empty domain list yields `None`: there is nothing to route, and
    /// `resolvectl domain <iface>` with zero domains does not clear existing
    /// ones, so applying an empty set would leave stale split-DNS active.
    /// Removing the last domain therefore reverts instead.
    fn desired(&self) -> Option<(VpnInfo, Vec<String>)> {
        let active = self.config.enabled && self.vpn_up && !self.config.vpn_hosts.is_empty();
        match (&self.last_info, active) {
            (Some(info), true) => Some((info.clone(), self.config.vpn_hosts.clone())),
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
                };
                if self.applied.as_ref() == Some(&target) {
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
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        // Leave `applied` as it was: a failed apply may have
                        // left the *previous* rules in place (e.g. the DNS
                        // step fails before apply_rules' rollback runs), so we
                        // must still remember to revert them later. Clearing it
                        // would make disable/down/shutdown skip the revert and
                        // leave stale DNS active.
                        log::error!("apply_rules failed on {}: {e}", info.interface_name);
                        Err(e)
                    }
                    Err(e) => {
                        log::error!("apply task panicked: {e}");
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
            return Ok(());
        };
        let backend = self.backend.clone();
        let interface = applied.interface.clone();
        let result = tokio::task::spawn_blocking(move || backend.revert_rules(&interface)).await;
        match result {
            Ok(Ok(())) => {
                log::info!("reverted rules on {}", applied.interface);
                self.applied = None;
                Ok(())
            }
            Ok(Err(e)) => {
                log::error!("revert_rules failed on {}: {e}", applied.interface);
                Err(e)
            }
            Err(e) => {
                log::error!("revert task panicked: {e}");
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
            return Response::Ok;
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
                if next.vpn_name != self.config.vpn_name {
                    log::warn!(
                        "config reload changed vpn_name {} -> {}; the interface change \
                         takes effect after a daemon restart",
                        self.config.vpn_name,
                        next.vpn_name
                    );
                }
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

    /// Records what the state machine asks the backend to do. `fail_apply` is
    /// atomic so a test can flip it after a first successful apply.
    #[derive(Default)]
    struct MockBackend {
        applies: Mutex<Vec<(String, Vec<String>)>>,
        reverts: Mutex<Vec<String>>,
        fail_apply: AtomicBool,
        fail_revert: bool,
    }

    impl MockBackend {
        fn set_fail_apply(&self, fail: bool) {
            self.fail_apply.store(fail, Ordering::Relaxed);
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
            if self.fail_revert {
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
    async fn failed_first_apply_leaves_state_unapplied() {
        let backend = Arc::new(MockBackend {
            fail_apply: AtomicBool::new(true),
            ..Default::default()
        });
        let mut sm = machine(backend.clone(), config(true, &["a.com"]), "apply-fails");

        sm.on_event(vpn_up("wg0")).await;

        // The very first apply failed and nothing was applied before, so there
        // is correctly nothing to revert later.
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
            fail_revert: true,
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

        sm.on_event(vpn_up("wg0")).await; // first apply fails (applied stays None)
        let resp = sm.on_request(Request::AddDomain("b.com".to_string())).await;

        // The re-apply fails, so the caller is told so rather than "ok"...
        assert!(matches!(resp, Response::Error(_)));
        // ...but the config change is still persisted to disk.
        let saved = config::load_config_from(&sm.config_path).unwrap();
        assert_eq!(saved.vpn_hosts, vec!["a.com", "b.com"]);
    }
}
