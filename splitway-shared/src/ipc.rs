//! IPC protocol shared by `splitway-daemon` and `splitway-cli` — the single
//! source of truth for the wire format so both sides cannot drift apart.
//!
//! Transport: a Unix domain socket. Wire format: newline-delimited JSON —
//! one [`RequestEnvelope`] object per line, to which the daemon replies with
//! exactly one [`Response`] line.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`Request`] / [`Response`]. The
/// daemon rejects envelopes whose version it does not speak.
pub const PROTOCOL_VERSION: u32 = 1;

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// The request succeeded and carries no data.
    Ok,
    /// Reply to [`Request::Status`].
    Status(StatusInfo),
    /// Reply to [`Request::ListDomains`].
    Domains(Vec<String>),
    /// The request failed; the string is a human-readable reason.
    Error(String),
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
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            let parsed: Response = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, response);
        }
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
