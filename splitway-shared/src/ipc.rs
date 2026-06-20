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
/// the [`Response::Config`] reply. Bumped to `3` in Phase 5 for the additive
/// `ListInterfaces` verb / [`Response::Interfaces`] reply and the richer
/// [`StatusInfo`] (the `applied` mapping, [`RoutingState`] and
/// [`DetectorHealth`] — all additive). Bumped to `4` in Phase 5c for the
/// additive [`RoutingState::ConfigInvalid`] variant (the malformed-config freeze
/// surfaced over IPC). The daemon enforces *strict equality* (see
/// `daemon::ipc::process_line`): a daemon rejects a client whose version differs,
/// and vice versa, so there is no silent mixed-version operation. The daemon, CLI
/// and GUI all build from this one workspace, so they upgrade in lockstep; a
/// mismatch only happens across separately-updated installs and is surfaced as
/// actionable "update splitway" guidance, never a raw decode error.
pub const PROTOCOL_VERSION: u32 = 4;

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
    /// Enumerate the host's network interfaces so a client can offer an
    /// interface picker without itself touching the platform or holding
    /// privileges. Read-only; replied to with [`Response::Interfaces`].
    ListInterfaces,
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
    /// Reply to [`Request::ListInterfaces`].
    Interfaces(Vec<InterfaceInfo>),
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
    /// The *configured* VPN interface name (`vpn_name`), not necessarily the one
    /// rules are applied to right now — see [`AppliedInfo::interface`]. Kept
    /// named `interface` for wire continuity; it is the configured device name.
    pub interface: String,
    /// Whether the VPN interface is currently up.
    pub vpn_up: bool,
    /// The DNS mapping currently applied to the system, or `None` when nothing
    /// is applied. `is_some()` recovers the old boolean "applied?" meaning while
    /// also answering "which domains route through which DNS, on which link".
    pub applied: Option<AppliedInfo>,
    /// A self-explaining summary of why routing is (or is not) active right now,
    /// mapped from the daemon's own reconcile decision — see [`RoutingState`].
    pub routing_state: RoutingState,
    /// Whether the VPN-detector watch is running, idle, or failed to start.
    pub detector_health: DetectorHealth,
    /// The configured domains.
    pub domains: Vec<String>,
}

/// A snapshot of the DNS rules currently applied to the system — the wire
/// projection of the daemon's internal applied state. Carried in
/// [`StatusInfo::applied`] so a client can *verify* what the daemon believes it
/// has installed (which domains route through which DNS, on which interface),
/// not just that *something* is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedInfo {
    /// The interface the rules are applied to (may differ from the configured
    /// [`StatusInfo::interface`] during a live interface switch).
    pub interface: String,
    /// The domains routed through the VPN DNS.
    pub domains: Vec<String>,
    /// The VPN DNS servers the domains are routed to.
    pub dns_servers: Vec<String>,
}

/// Why DNS routing is — or is not — active right now, mapped from the daemon's
/// reconcile decision. This is *belief*: what the daemon intends given its
/// config and the observed VPN state, not a read-back of the live system
/// (reality / drift detection is a later phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingState {
    /// Rule application is disabled (`enabled = false`).
    Disabled,
    /// No domains are configured, so there is nothing to route.
    NoDomains,
    /// The configured VPN interface is not up.
    VpnDown,
    /// The VPN is up but exposes no DNS servers to route the domains to.
    NoDnsFromVpn,
    /// Rules are applied — the daemon has installed its intended mapping. This
    /// is belief, not a read-back: it means the apply call succeeded, not that
    /// the live system has been re-verified (drift detection is a later phase).
    Applied,
    /// The last apply (or revert) failed, so the system may be out of sync and
    /// a re-apply is pending.
    ApplyFailed,
    /// The config file on disk does not parse (a malformed hand-edit). Routing
    /// reflects the last-good config the daemon froze on; this is the
    /// highest-precedence state, and it clears automatically once the file
    /// parses again. See the daemon's `on_config_changed`.
    ConfigInvalid,
}

/// Health of the VPN-detector watch, set by the daemon when it (re-)arms the
/// watch and reported in [`StatusInfo::detector_health`]. Lets a client say
/// "the watch is up / idle / failed to start" instead of inferring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectorHealth {
    /// A detector watch is running for the configured interface.
    Active,
    /// No watch is running because no interface is configured (`vpn_name` is
    /// empty). Auto-apply is intentionally off, not broken.
    Inactive,
    /// The watch failed to start (e.g. NetworkManager absent, or a bad OpenVPN
    /// management endpoint). Auto-apply is off; the string is the reason.
    Error(String),
}

/// One enumerated network interface, for the client's interface picker. A small
/// dedicated wire type (like [`ConfigView`]) so the picker need not touch the
/// platform or hold privileges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceInfo {
    /// The interface (device) name, e.g. `tun0`, `eth0`, `lo`.
    pub name: String,
    /// Whether the interface is currently up.
    pub up: bool,
    /// A name-prefix heuristic flag (`tun*` / `utun*` / `wg*` / `tap*` / `ppp*`
    /// / `gpd*`) hinting this is VPN-like. Advisory only — a client may use it
    /// to sort or highlight, never to filter the list.
    pub vpn_like: bool,
}

/// Concise human phrasing for [`StatusInfo::applied`], shared by the CLI and the
/// daemon's own `status` subcommand: `wg0 -> [a.com, b.com] via [10.0.0.1]`.
impl std::fmt::Display for AppliedInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -> [{}] via [{}]",
            self.interface,
            self.domains.join(", "),
            self.dns_servers.join(", ")
        )
    }
}

/// Human phrasing for [`StatusInfo::routing_state`], shared across clients.
impl std::fmt::Display for RoutingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            RoutingState::Disabled => "disabled",
            RoutingState::NoDomains => "no domains configured",
            RoutingState::VpnDown => "VPN down",
            RoutingState::NoDnsFromVpn => "VPN up, but it pushes no DNS",
            RoutingState::Applied => "applied",
            RoutingState::ApplyFailed => "apply failed (out of sync)",
            RoutingState::ConfigInvalid => "config file invalid (using last-good)",
        };
        f.write_str(text)
    }
}

/// Human phrasing for [`StatusInfo::detector_health`], shared across clients.
impl std::fmt::Display for DetectorHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectorHealth::Active => f.write_str("active"),
            DetectorHealth::Inactive => f.write_str("inactive (no interface configured)"),
            DetectorHealth::Error(reason) => write!(f, "error: {reason}"),
        }
    }
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

    fn sample_status() -> StatusInfo {
        StatusInfo {
            enabled: true,
            interface: "wg0".to_string(),
            vpn_up: true,
            applied: Some(AppliedInfo {
                interface: "wg0".to_string(),
                domains: vec!["a.com".to_string(), "b.com".to_string()],
                dns_servers: vec!["10.0.0.1".to_string()],
            }),
            routing_state: RoutingState::Applied,
            detector_health: DetectorHealth::Active,
            domains: vec!["a.com".to_string(), "b.com".to_string()],
        }
    }

    #[test]
    fn response_round_trip() {
        let responses = [
            Response::Ok,
            Response::Domains(vec!["a.com".to_string(), "b.com".to_string()]),
            Response::Error("nope".to_string()),
            Response::Status(sample_status()),
            // Not-applied status: `applied` is None, with a non-Applied state and
            // a failed detector (exercises every new field's other shape).
            Response::Status(StatusInfo {
                enabled: true,
                interface: "tun0".to_string(),
                vpn_up: false,
                applied: None,
                routing_state: RoutingState::VpnDown,
                detector_health: DetectorHealth::Error("nm absent".to_string()),
                domains: vec![],
            }),
            Response::Config(sample_config_view()),
            Response::Interfaces(vec![
                InterfaceInfo {
                    name: "tun0".to_string(),
                    up: true,
                    vpn_like: true,
                },
                InterfaceInfo {
                    name: "lo".to_string(),
                    up: true,
                    vpn_like: false,
                },
            ]),
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            let parsed: Response = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, response);
        }
    }

    #[test]
    fn list_interfaces_verb_round_trips_in_envelope() {
        let env = RequestEnvelope::new(Request::ListInterfaces);
        let json = serde_json::to_string(&env).unwrap();
        let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(parsed.request, Request::ListInterfaces);
    }

    #[test]
    fn new_status_wire_types_round_trip() {
        // Each new type round-trips on its own, like ConfigView does.
        let applied = AppliedInfo {
            interface: "utun4".to_string(),
            domains: vec!["corp.example".to_string()],
            dns_servers: vec!["10.8.0.1".to_string(), "10.8.0.2".to_string()],
        };
        let json = serde_json::to_string(&applied).unwrap();
        assert_eq!(serde_json::from_str::<AppliedInfo>(&json).unwrap(), applied);

        for state in [
            RoutingState::Disabled,
            RoutingState::NoDomains,
            RoutingState::VpnDown,
            RoutingState::NoDnsFromVpn,
            RoutingState::Applied,
            RoutingState::ApplyFailed,
            RoutingState::ConfigInvalid,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<RoutingState>(&json).unwrap(), state);
        }

        for health in [
            DetectorHealth::Active,
            DetectorHealth::Inactive,
            DetectorHealth::Error("bad management endpoint".to_string()),
        ] {
            let json = serde_json::to_string(&health).unwrap();
            assert_eq!(
                serde_json::from_str::<DetectorHealth>(&json).unwrap(),
                health
            );
        }

        let iface = InterfaceInfo {
            name: "wg0".to_string(),
            up: false,
            vpn_like: true,
        };
        let json = serde_json::to_string(&iface).unwrap();
        assert_eq!(serde_json::from_str::<InterfaceInfo>(&json).unwrap(), iface);
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
