//! IPC protocol shared by `splitway-daemon` and `splitway-cli` — the single
//! source of truth for the wire format so both sides cannot drift apart.
//!
//! Transport: a Unix domain socket. Wire format: newline-delimited JSON —
//! one [`RequestEnvelope`] object per line, to which the daemon replies with
//! exactly one [`Response`] line.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::VpnBackend;

/// Bumped on any incompatible change to [`Request`] / [`Response`]. The
/// daemon rejects envelopes whose version it does not speak.
///
/// Bumped to `2` in Phase 4 for the additive `GetConfig`/`SetConfig` verbs and
/// the [`Response::Config`] reply. The daemon enforces *strict equality* (see
/// `daemon::ipc::process_line`): a v2 daemon rejects a v1 client and vice
/// versa, so there is no silent mixed-version operation. The daemon, CLI and
/// GUI all build from this one workspace, so they upgrade in lockstep; a
/// mismatch only happens across separately-updated installs and is surfaced as
/// actionable "update splitway" guidance, never a raw decode error.
pub const PROTOCOL_VERSION: u32 = 2;

/// Stable prefix the daemon uses to introduce a protocol-version-mismatch
/// error reply. Shared so a client (CLI/GUI) can recognize skew and render
/// "update splitway" guidance distinctly, instead of string-matching a literal
/// that could drift from the daemon's wording.
pub const VERSION_MISMATCH_PREFIX: &str = "protocol version mismatch";

/// Versioned wrapper around a [`Request`]. Carrying the version per request
/// keeps a single-shot client trivial — no separate handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub version: u32,
    pub request: Request,
}

impl RequestEnvelope {
    pub fn new(request: Request) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Current daemon + DNS routing state.
    Status,
    /// Enable rule application (persisted).
    Enable,
    /// Disable rule application and revert (persisted).
    Disable,
    /// Add a domain to route through the VPN (persisted).
    AddDomain(String),
    /// Remove a domain (persisted). Absent domain is a no-op success.
    RemoveDomain(String),
    /// List the configured domains.
    ListDomains,
    /// Re-read the config file from disk and reconcile.
    ReloadConfig,
    /// Read the editable config projection (the settings not covered by the
    /// other verbs). Replied to with [`Response::Config`].
    GetConfig,
    /// Update the editable config projection (persisted). The daemon handles
    /// this in its single-writer state actor via the same `commit()` path as
    /// the other mutating verbs, so it stays the sole config writer.
    SetConfig(ConfigView),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// The request succeeded and carries no data.
    Ok,
    /// Reply to [`Request::Status`].
    Status(StatusInfo),
    /// Reply to [`Request::ListDomains`].
    Domains(Vec<String>),
    /// Reply to [`Request::GetConfig`].
    Config(ConfigView),
    /// The request failed; the string is a human-readable reason.
    Error(String),
}

/// The editable projection of `LocalConfig` carried over IPC — deliberately a
/// small, dedicated wire type rather than `LocalConfig` itself, so the wire
/// format stays independently versionable and is not coupled to the on-disk
/// serde layout.
///
/// It covers exactly the gap the other verbs leave: `vpn_name`, `vpn_backend`
/// and the `openvpn.*` settings. `enabled` stays owned by `Enable`/`Disable`
/// and the domain list by `AddDomain`/`RemoveDomain`/`ListDomains`, so
/// [`Request::SetConfig`] is a *partial* update that never clobbers another
/// verb's slice of the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigView {
    /// The configured VPN interface (device) name.
    pub vpn_name: String,
    /// Which Linux VPN detector to use. `#[serde(default)]` keeps the wire type
    /// forward-compatible if the field is ever omitted by an older peer.
    #[serde(default)]
    pub vpn_backend: VpnBackend,
    /// Standalone-OpenVPN management endpoint (`host:port` or a unix socket
    /// path); ignored unless `vpn_backend = openvpn`.
    #[serde(default)]
    pub openvpn_management: String,
    /// Optional path to the management password file. `None` = no password.
    #[serde(default)]
    pub openvpn_management_password_file: Option<String>,
    /// Read-only: the daemon's effective config file path. The daemon fills
    /// this on [`Request::GetConfig`] and *ignores* it on
    /// [`Request::SetConfig`] — the active path is fixed at daemon launch
    /// (via `--config`), so the GUI cannot switch it at runtime.
    #[serde(default)]
    pub config_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    /// Whether rule application is enabled.
    pub enabled: bool,
    /// The configured VPN interface name.
    pub interface: String,
    /// Whether the VPN interface is currently up.
    pub vpn_up: bool,
    /// Whether DNS rules are currently applied to the system.
    pub applied: bool,
    /// The configured domains.
    pub domains: Vec<String>,
}

/// System-service socket directory, used when `XDG_RUNTIME_DIR` is unset (a
/// root service rather than a login session). macOS has no `/run` and a
/// read-only root volume, so the daemon (which creates this dir on bind) uses
/// `/var/run`; Linux keeps `/run` (systemd provisions `/run/splitway`).
#[cfg(target_os = "macos")]
const SYSTEM_SOCKET_DIR: &str = "/var/run/splitway";
#[cfg(not(target_os = "macos"))]
const SYSTEM_SOCKET_DIR: &str = "/run/splitway";

/// Resolve the control socket path: `$XDG_RUNTIME_DIR/splitway.sock` when
/// the runtime dir is set (already a `0700` user-private directory), else the
/// per-platform [`SYSTEM_SOCKET_DIR`] for a system service.
pub fn socket_path() -> PathBuf {
    socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Pure resolver split out so the `XDG_RUNTIME_DIR` preference is unit-testable
/// without mutating the process-global environment.
fn socket_path_from(runtime_dir: Option<OsString>) -> PathBuf {
    match runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("splitway.sock"),
        _ => PathBuf::from(SYSTEM_SOCKET_DIR).join("splitway.sock"),
    }
}

/// Synchronous single-shot client used by `splitway-cli` and the
/// `splitway-daemon status` subcommand: connect, send one request, read one
/// response. Single-shot, so a blocking `UnixStream` is simpler and needs no
/// async runtime.
#[cfg(unix)]
pub mod client {
    use super::{socket_path, Request, RequestEnvelope, Response};
    use std::fmt;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    #[derive(Debug)]
    pub enum ClientError {
        /// The socket could not be reached — most likely the daemon is down.
        NotRunning(std::io::Error),
        /// The socket exists but the caller is not allowed to connect — the
        /// daemon is running, but the user lacks access to its control socket.
        PermissionDenied(std::io::Error),
        /// An I/O error after connecting.
        Io(std::io::Error),
        /// A malformed or unexpected reply.
        Protocol(String),
    }

    impl fmt::Display for ClientError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                ClientError::NotRunning(e) => write!(
                    f,
                    "cannot reach the splitway daemon socket ({e}); is splitway-daemon running?"
                ),
                ClientError::PermissionDenied(e) => write!(
                    f,
                    "permission denied on the splitway daemon socket ({e}); \
                     the daemon is running but you lack access — try sudo or the daemon's group"
                ),
                ClientError::Io(e) => write!(f, "IPC I/O error: {e}"),
                ClientError::Protocol(m) => write!(f, "IPC protocol error: {m}"),
            }
        }
    }

    impl std::error::Error for ClientError {}

    /// Candidate sockets to try, in order: the per-user socket (if
    /// `$XDG_RUNTIME_DIR` is set) then the system socket. A login-session CLI
    /// thus reaches a system-service daemon (which binds [`SYSTEM_SOCKET_DIR`])
    /// even though its own `socket_path()` resolves to `$XDG_RUNTIME_DIR`.
    fn candidate_sockets() -> Vec<std::path::PathBuf> {
        let mut paths = vec![socket_path()];
        let system = std::path::PathBuf::from(super::SYSTEM_SOCKET_DIR).join("splitway.sock");
        if !paths.contains(&system) {
            paths.push(system);
        }
        paths
    }

    /// Connect to the daemon socket, send `request`, and return its reply.
    pub fn send_request(request: Request) -> Result<Response, ClientError> {
        let mut stream = None;
        let mut last_err = None;
        let mut permission_denied = None;
        for path in candidate_sockets() {
            match UnixStream::connect(&path) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                // A 0600 socket owned by another user (e.g. the root system
                // daemon) reports PermissionDenied — the daemon *is* running.
                // Keep it regardless of candidate order, so it is never masked
                // by a NotFound from a different candidate.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    permission_denied = Some(e);
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = match stream {
            Some(connected) => connected,
            None => {
                if let Some(e) = permission_denied {
                    return Err(ClientError::PermissionDenied(e));
                }
                let err = last_err.unwrap_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "no socket candidates")
                });
                return Err(ClientError::NotRunning(err));
            }
        };

        let mut writer = stream.try_clone().map_err(ClientError::Io)?;
        let mut line = serde_json::to_string(&RequestEnvelope::new(request))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        writer.write_all(line.as_bytes()).map_err(ClientError::Io)?;
        writer.flush().map_err(ClientError::Io)?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        if reader
            .read_line(&mut response_line)
            .map_err(ClientError::Io)?
            == 0
        {
            return Err(ClientError::Protocol(
                "daemon closed the connection without replying".to_string(),
            ));
        }
        serde_json::from_str(response_line.trim_end())
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config_view() -> ConfigView {
        ConfigView {
            vpn_name: "tun0".to_string(),
            vpn_backend: VpnBackend::OpenVpn,
            openvpn_management: "127.0.0.1:7505".to_string(),
            openvpn_management_password_file: Some("/etc/splitway/mgmt.pass".to_string()),
            config_path: "/home/user/.config/splitway/config.json".to_string(),
        }
    }

    #[test]
    fn envelope_round_trip_carries_version() {
        let env = RequestEnvelope::new(Request::AddDomain("example.com".to_string()));
        let json = serde_json::to_string(&env).unwrap();
        let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(
            parsed.request,
            Request::AddDomain("example.com".to_string())
        );
    }

    #[test]
    fn config_verbs_round_trip_in_envelope() {
        // The new GetConfig / SetConfig verbs ride the same versioned envelope.
        for request in [Request::GetConfig, Request::SetConfig(sample_config_view())] {
            let env = RequestEnvelope::new(request.clone());
            let json = serde_json::to_string(&env).unwrap();
            let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.version, PROTOCOL_VERSION);
            assert_eq!(parsed.request, request);
        }
    }

    #[test]
    fn response_round_trip() {
        let responses = [
            Response::Ok,
            Response::Domains(vec!["a.com".to_string(), "b.com".to_string()]),
            Response::Error("nope".to_string()),
            Response::Status(StatusInfo {
                enabled: true,
                interface: "wg0".to_string(),
                vpn_up: false,
                applied: false,
                domains: vec!["a.com".to_string()],
            }),
            Response::Config(sample_config_view()),
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            let parsed: Response = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, response);
        }
    }

    #[test]
    fn config_view_round_trips() {
        let view = sample_config_view();
        let json = serde_json::to_string(&view).unwrap();
        let parsed: ConfigView = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, view);
    }

    #[test]
    fn config_view_optional_fields_default_when_absent() {
        // Mirror the LocalConfig back-compat discipline: a peer that omits the
        // optional fields still parses, with the defaults applied.
        let json = r#"{"vpn_name":"wg0"}"#;
        let parsed: ConfigView = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.vpn_name, "wg0");
        assert_eq!(parsed.vpn_backend, VpnBackend::NetworkManager);
        assert!(parsed.openvpn_management.is_empty());
        assert!(parsed.openvpn_management_password_file.is_none());
        assert!(parsed.config_path.is_empty());
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        // A non-empty XDG_RUNTIME_DIR places the socket directly under it.
        assert_eq!(
            socket_path_from(Some(OsString::from("/run/user/1000"))),
            PathBuf::from("/run/user/1000/splitway.sock")
        );
    }

    #[test]
    fn socket_path_falls_back_to_system_dir_without_xdg() {
        // An unset or empty XDG_RUNTIME_DIR falls back to the system socket dir.
        let expected = PathBuf::from(SYSTEM_SOCKET_DIR).join("splitway.sock");
        assert_eq!(socket_path_from(None), expected);
        assert_eq!(socket_path_from(Some(OsString::new())), expected);
    }
}
