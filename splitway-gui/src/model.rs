//! Pure view-model logic for the GUI: it turns daemon replies and client
//! errors into what the widgets display, and validates user input before a
//! request is sent. No egui, no IPC, no threads live here, so it is unit-tested
//! in isolation — mirroring the repo's split of pure logic (tested) from thin
//! plumbing (`app.rs`/`worker.rs`, untested).

use splitway_shared::config::VpnBackend;
use splitway_shared::ipc::client::ClientError;
use splitway_shared::ipc::{
    AppliedInfo, ConfigView, InterfaceInfo, Request, Response, VERSION_MISMATCH_PREFIX,
};

/// Health of the link to the daemon, classified from the most recent
/// round-trip. Drives the connection banner and whether the live status block
/// is trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// No round-trip has completed yet.
    Unknown,
    /// The last round-trip succeeded.
    Connected,
    /// The socket is unreachable — most likely the daemon is not running.
    NotRunning,
    /// The socket is reachable but access was denied (a root daemon seen by an
    /// unprivileged GUI). The GUI never escalates.
    PermissionDenied,
    /// The daemon and client speak different protocol versions — the user
    /// should update splitway.
    VersionMismatch,
    /// Any other transient failure (I/O, malformed reply, unexpected error).
    TransientError,
}

/// The connection banner the UI renders, reduced from one round-trip result.
/// `message` is `None` only when healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionState {
    pub health: Health,
    pub message: Option<String>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        ConnectionState {
            health: Health::Unknown,
            message: None,
        }
    }
}

/// Classify a transport-level [`ClientError`] into a [`Health`]. The error's
/// own `Display` text is reused verbatim for the message (see
/// [`reduce_status_result`]); the existing client guidance — including the
/// "run as the daemon's user/group" note for `PermissionDenied` — is not
/// re-worded here.
pub fn classify_client_error(err: &ClientError) -> Health {
    match err {
        ClientError::NotRunning(_) => Health::NotRunning,
        ClientError::PermissionDenied(_) => Health::PermissionDenied,
        ClientError::Io(_) | ClientError::Protocol(_) => Health::TransientError,
    }
}

/// Whether a daemon `Response::Error` text is a protocol-version mismatch,
/// detected via the shared [`VERSION_MISMATCH_PREFIX`] (not a fragile literal).
pub fn is_version_mismatch(error_message: &str) -> bool {
    error_message.starts_with(VERSION_MISMATCH_PREFIX)
}

/// Reduce the result of a `Status` round-trip into the connection banner. A
/// version-mismatch `Response::Error` is flagged distinctly so the UI can show
/// "update" guidance rather than a raw error; every other error reuses the
/// daemon/client text verbatim.
pub fn reduce_status_result(result: &Result<Response, ClientError>) -> ConnectionState {
    match result {
        Ok(Response::Status(_)) => ConnectionState {
            health: Health::Connected,
            message: None,
        },
        Ok(Response::Error(msg)) if is_version_mismatch(msg) => ConnectionState {
            health: Health::VersionMismatch,
            message: Some(msg.clone()),
        },
        Ok(Response::Error(msg)) => ConnectionState {
            health: Health::TransientError,
            message: Some(msg.clone()),
        },
        Ok(other) => ConnectionState {
            health: Health::TransientError,
            message: Some(format!("unexpected reply from daemon: {other:?}")),
        },
        Err(err) => ConnectionState {
            health: classify_client_error(err),
            message: Some(err.to_string()),
        },
    }
}

/// The outcome of a mutating action (enable/disable, add/remove domain, save
/// config), reduced to a single user-facing line. `Ok` carries a success note,
/// `Err` an error note; both are shown as a dismissable message.
pub fn reduce_action_result(
    action: &str,
    result: &Result<Response, ClientError>,
) -> Result<String, String> {
    match result {
        Ok(Response::Ok) => Ok(format!("{action}: done")),
        Ok(Response::Error(msg)) => Err(format!("{action}: {msg}")),
        Ok(other) => Err(format!("{action}: unexpected reply: {other:?}")),
        Err(err) => Err(format!("{action}: {err}")),
    }
}

/// Validate and normalize a domain before it is sent. The daemon remains the
/// source of truth (it rejects duplicates and persists), but obvious garbage is
/// caught here so the user gets immediate feedback. Returns the trimmed domain.
pub fn validate_domain(input: &str) -> Result<String, String> {
    let domain = input.trim();
    if domain.is_empty() {
        return Err("domain must not be empty".to_string());
    }
    if domain.chars().any(char::is_whitespace) {
        return Err("domain must not contain whitespace".to_string());
    }
    if domain.contains('/') || domain.contains(':') {
        return Err("enter a bare domain — no scheme, port, or path".to_string());
    }
    // Dot-separated labels of ASCII letters, digits and hyphens. This rejects
    // empty labels (leading/trailing/doubled dots) too.
    let labels_ok = domain.split('.').all(|label| {
        !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    if !labels_ok {
        return Err("invalid domain — use letters, digits, hyphens and dots".to_string());
    }
    Ok(domain.to_string())
}

/// Validate the editable config before sending [`SetConfig`]. Kept minimal and
/// aligned with the daemon's own semantics: an empty `vpn_name` is allowed (the
/// daemon warns and stays controllable), but the OpenVPN backend has no usable
/// configuration without a management endpoint.
///
/// [`SetConfig`]: splitway_shared::ipc::Request::SetConfig
pub fn validate_config_fields(view: &ConfigView) -> Result<(), String> {
    if view.vpn_backend == VpnBackend::OpenVpn && view.openvpn_management.trim().is_empty() {
        return Err(
            "the OpenVPN backend needs a management endpoint (host:port or a unix socket path)"
                .to_string(),
        );
    }
    Ok(())
}

/// One entry in the GUI's interface picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceChoice {
    /// The interface name to write into `vpn_name` if selected.
    pub name: String,
    /// The display label, e.g. `tun0 (up, vpn)` or `eth0 (down)`.
    pub label: String,
    /// True when this entry is the currently-configured interface that the
    /// daemon did not enumerate (absent or down) — kept so the user's choice is
    /// never dropped from the picker just because the VPN is not up right now.
    pub configured_but_absent: bool,
}

/// Build the interface-picker entries from the daemon's enumerated interfaces
/// plus the currently-configured `vpn_name`. The live interfaces come first
/// (already sorted up-first / vpn-like-first by the daemon). If the configured
/// name is non-empty and not among them it is appended so it stays visible and
/// selectable; an empty configured name adds no synthetic entry.
///
/// "Not among them" is split two ways, so an enumeration failure is never
/// reported as a missing interface:
/// - the daemon returned interfaces but not this one → it is genuinely *not
///   present* (a VPN that is down right now), flagged `configured_but_absent`;
/// - the list is **empty** (enumeration unavailable / not yet fetched) → we have
///   no inventory to judge presence, so it is labelled just "(configured)" and
///   not flagged absent. The editor's free-text field remains the fallback.
pub fn interface_choices(interfaces: &[InterfaceInfo], configured: &str) -> Vec<InterfaceChoice> {
    let mut choices: Vec<InterfaceChoice> = interfaces
        .iter()
        .map(|iface| InterfaceChoice {
            name: iface.name.clone(),
            label: interface_label(iface),
            configured_but_absent: false,
        })
        .collect();

    let configured = configured.trim();
    if !configured.is_empty() && !interfaces.iter().any(|iface| iface.name == configured) {
        // Only claim "not present" when we actually have an inventory to check
        // against; an empty list means we have no data, not that it is absent.
        let (label, configured_but_absent) = if interfaces.is_empty() {
            (format!("{configured} (configured)"), false)
        } else {
            (format!("{configured} (configured, not present)"), true)
        };
        choices.push(InterfaceChoice {
            name: configured.to_string(),
            label,
            configured_but_absent,
        });
    }
    choices
}

/// The display label for one enumerated interface: name plus up/down and a `vpn`
/// hint when the daemon flagged it VPN-like.
fn interface_label(iface: &InterfaceInfo) -> String {
    let state = if iface.up { "up" } else { "down" };
    if iface.vpn_like {
        format!("{} ({state}, vpn)", iface.name)
    } else {
        format!("{} ({state})", iface.name)
    }
}

/// The requests that refresh the view after a successful mutation or a resync:
/// always `Status` + `GetConfig`, plus `ListInterfaces` when the interface set
/// or selection may have changed (`include_interfaces` — a config save or a
/// resync, but not enable/disable/add/remove, which never touch the interfaces).
pub fn refresh_requests(include_interfaces: bool) -> Vec<Request> {
    let mut requests = vec![Request::Status, Request::GetConfig];
    if include_interfaces {
        requests.push(Request::ListInterfaces);
    }
    requests
}

/// A one-line summary of [`StatusInfo::applied`][applied] for the status block:
/// the interface → domains → DNS mapping when applied, else a plain note. The
/// only decision here is `None` vs `Some`; the mapping itself reuses
/// `AppliedInfo`'s shared `Display`.
///
/// [applied]: splitway_shared::ipc::StatusInfo::applied
pub fn applied_summary(applied: &Option<AppliedInfo>) -> String {
    match applied {
        None => "(nothing applied)".to_string(),
        Some(applied) => applied.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn err(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "test")
    }

    fn view(backend: VpnBackend, management: &str) -> ConfigView {
        ConfigView {
            vpn_name: "tun0".to_string(),
            vpn_backend: backend,
            openvpn_management: management.to_string(),
            openvpn_management_password_file: None,
            config_path: "/tmp/c.json".to_string(),
        }
    }

    #[test]
    fn client_errors_classify_to_health() {
        assert_eq!(
            classify_client_error(&ClientError::NotRunning(err(io::ErrorKind::NotFound))),
            Health::NotRunning
        );
        assert_eq!(
            classify_client_error(&ClientError::PermissionDenied(err(
                io::ErrorKind::PermissionDenied
            ))),
            Health::PermissionDenied
        );
        assert_eq!(
            classify_client_error(&ClientError::Io(err(io::ErrorKind::BrokenPipe))),
            Health::TransientError
        );
        assert_eq!(
            classify_client_error(&ClientError::Protocol("bad".to_string())),
            Health::TransientError
        );
    }

    #[test]
    fn status_result_reduces_to_connected() {
        use splitway_shared::ipc::{DetectorHealth, RoutingState, StatusInfo};
        let ok = Ok(Response::Status(StatusInfo {
            enabled: true,
            interface: "wg0".to_string(),
            vpn_up: true,
            applied: None,
            routing_state: RoutingState::VpnDown,
            detector_health: DetectorHealth::Active,
            domains: vec![],
        }));
        let state = reduce_status_result(&ok);
        assert_eq!(state.health, Health::Connected);
        assert!(state.message.is_none());
    }

    #[test]
    fn not_running_degrades_with_message() {
        let result = Err(ClientError::NotRunning(err(io::ErrorKind::NotFound)));
        let state = reduce_status_result(&result);
        assert_eq!(state.health, Health::NotRunning);
        // The client's own guidance is surfaced verbatim.
        assert!(state.message.unwrap().contains("splitway-daemon running"));
    }

    #[test]
    fn version_mismatch_response_is_flagged() {
        let msg = format!("{VERSION_MISMATCH_PREFIX}: daemon speaks 2, client speaks 1 — update");
        let result: Result<Response, ClientError> = Ok(Response::Error(msg.clone()));
        let state = reduce_status_result(&result);
        assert_eq!(state.health, Health::VersionMismatch);
        assert_eq!(state.message.as_deref(), Some(msg.as_str()));
        assert!(is_version_mismatch(&msg));
        assert!(!is_version_mismatch("domain already present: a.com"));
    }

    #[test]
    fn generic_response_error_is_transient() {
        let result: Result<Response, ClientError> =
            Ok(Response::Error("domain already present: a.com".to_string()));
        let state = reduce_status_result(&result);
        assert_eq!(state.health, Health::TransientError);
        assert_eq!(
            state.message.as_deref(),
            Some("domain already present: a.com")
        );
    }

    #[test]
    fn action_result_messages() {
        assert_eq!(
            reduce_action_result("enable", &Ok(Response::Ok)),
            Ok("enable: done".to_string())
        );
        assert_eq!(
            reduce_action_result(
                "add domain",
                &Ok(Response::Error("domain already present: a.com".to_string()))
            ),
            Err("add domain: domain already present: a.com".to_string())
        );
        let net = Err(ClientError::NotRunning(err(io::ErrorKind::NotFound)));
        assert!(reduce_action_result("save config", &net)
            .unwrap_err()
            .starts_with("save config: "));
    }

    #[test]
    fn domain_validation_accepts_reasonable_domains() {
        assert_eq!(
            validate_domain("  corp.example.com "),
            Ok("corp.example.com".to_string())
        );
        assert_eq!(
            validate_domain("internal-host.corp"),
            Ok("internal-host.corp".to_string())
        );
        assert_eq!(validate_domain("localhost"), Ok("localhost".to_string()));
    }

    #[test]
    fn domain_validation_rejects_garbage() {
        assert!(validate_domain("").is_err());
        assert!(validate_domain("   ").is_err());
        assert!(validate_domain("has space.com").is_err());
        assert!(validate_domain("https://corp.example.com").is_err());
        assert!(validate_domain("corp.example.com:443").is_err());
        assert!(validate_domain("trailing.dot.").is_err());
        assert!(validate_domain(".leading.dot").is_err());
        assert!(validate_domain("double..dot").is_err());
        assert!(validate_domain("under_score.com").is_err());
    }

    #[test]
    fn config_validation_requires_management_for_openvpn() {
        assert!(validate_config_fields(&view(VpnBackend::OpenVpn, "")).is_err());
        assert!(validate_config_fields(&view(VpnBackend::OpenVpn, "   ")).is_err());
        assert!(validate_config_fields(&view(VpnBackend::OpenVpn, "127.0.0.1:7505")).is_ok());
        // NetworkManager has no such requirement.
        assert!(validate_config_fields(&view(VpnBackend::NetworkManager, "")).is_ok());
    }

    fn iface(name: &str, up: bool, vpn_like: bool) -> InterfaceInfo {
        InterfaceInfo {
            name: name.to_string(),
            up,
            vpn_like,
        }
    }

    #[test]
    fn interface_choices_keep_daemon_order_and_label_state() {
        let interfaces = [iface("tun0", true, true), iface("eth0", false, false)];
        let choices = interface_choices(&interfaces, "tun0");
        // The configured interface is present, so no synthetic entry is added.
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].name, "tun0");
        assert_eq!(choices[0].label, "tun0 (up, vpn)");
        assert!(!choices[0].configured_but_absent);
        assert_eq!(choices[1].label, "eth0 (down)");
    }

    #[test]
    fn interface_choices_keep_a_configured_but_absent_interface() {
        // The configured VPN is not up right now, so the daemon did not list it.
        let interfaces = [iface("eth0", true, false)];
        let choices = interface_choices(&interfaces, "tun7");
        assert_eq!(choices.len(), 2);
        let configured = choices.last().unwrap();
        assert_eq!(configured.name, "tun7");
        assert!(configured.configured_but_absent);
        assert!(configured.label.contains("not present"));
    }

    #[test]
    fn interface_choices_add_nothing_for_an_empty_or_listed_name() {
        // Empty configured name -> no synthetic entry.
        assert_eq!(
            interface_choices(&[iface("eth0", true, false)], "   ").len(),
            1
        );
        // Whitespace-padded configured name that IS listed -> no duplicate.
        assert_eq!(
            interface_choices(&[iface("tun0", true, true)], " tun0 ").len(),
            1
        );
    }

    #[test]
    fn interface_choices_does_not_claim_absent_when_inventory_is_empty() {
        // Enumeration unavailable (empty list): the configured value is still
        // shown so it stays selectable, but it must NOT be labelled "not present"
        // or flagged absent — we have no inventory to judge it against.
        let choices = interface_choices(&[], "tun0");
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].name, "tun0");
        assert_eq!(choices[0].label, "tun0 (configured)");
        assert!(!choices[0].configured_but_absent);
        assert!(!choices[0].label.contains("not present"));
    }

    #[test]
    fn refresh_requests_include_interfaces_only_when_asked() {
        assert_eq!(
            refresh_requests(false),
            vec![Request::Status, Request::GetConfig]
        );
        assert_eq!(
            refresh_requests(true),
            vec![Request::Status, Request::GetConfig, Request::ListInterfaces]
        );
    }

    #[test]
    fn applied_summary_reads_none_and_the_mapping() {
        assert_eq!(applied_summary(&None), "(nothing applied)");
        let applied = Some(AppliedInfo {
            interface: "wg0".to_string(),
            domains: vec!["a.com".to_string(), "b.com".to_string()],
            dns_servers: vec!["10.0.0.1".to_string()],
        });
        assert_eq!(
            applied_summary(&applied),
            "wg0 -> [a.com, b.com] via [10.0.0.1]"
        );
    }
}
