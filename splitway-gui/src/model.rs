//! Pure view-model logic for the GUI: it turns daemon replies and client
//! errors into what the widgets display, and validates user input before a
//! request is sent. No egui, no IPC, no threads live here, so it is unit-tested
//! in isolation — mirroring the repo's split of pure logic (tested) from thin
//! plumbing (`app.rs`/`worker.rs`, untested).

use splitway_shared::config::VpnBackend;
use splitway_shared::ipc::client::ClientError;
use splitway_shared::ipc::{ConfigView, Response, VERSION_MISMATCH_PREFIX};

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

/// Whether the edited `vpn_name` differs from the daemon's currently-active
/// interface — i.e. there is an unsaved interface change. The detector watch is
/// armed once at startup and is not restarted on a live config change, so such a
/// change needs a daemon restart to auto-apply on the new interface; the UI uses
/// this to flag the pending change. (`vpn_backend` carries the same restart
/// caveat, but the live backend is not exposed in `StatusInfo`, so the UI
/// surfaces that part as an always-on note rather than a diff against live.)
pub fn interface_change_needs_restart(edited_vpn_name: &str, live_interface: &str) -> bool {
    edited_vpn_name.trim() != live_interface
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
        use splitway_shared::ipc::StatusInfo;
        let ok = Ok(Response::Status(StatusInfo {
            enabled: true,
            interface: "wg0".to_string(),
            vpn_up: true,
            applied: true,
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
            validate_domain("internal-host.example"),
            Ok("internal-host.example".to_string())
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

    #[test]
    fn restart_caveat_triggers_on_interface_change() {
        assert!(interface_change_needs_restart("tun1", "tun0"));
        assert!(!interface_change_needs_restart("tun0", "tun0"));
        assert!(!interface_change_needs_restart("  tun0 ", "tun0"));
    }
}
