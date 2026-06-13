//! Unix-domain-socket IPC server. Accepts newline-delimited JSON requests
//! and funnels each one into the single state-owner task via an `mpsc` +
//! `oneshot` reply channel. A malformed request is logged and answered with
//! an error response — never a panic.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use splitway_shared::ipc::{RequestEnvelope, Response, PROTOCOL_VERSION};

use crate::daemon::state::StateCommand;

/// Restores the previous process umask when dropped, so a tightened umask
/// around `bind()` cannot leak to the rest of the process (including on the
/// error path).
struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    /// Tighten the umask to `0o177` so a freshly created file gets mode
    /// `0600` (owner read/write only).
    fn owner_only() -> Self {
        // SAFETY: `umask` is always safe to call; it just swaps a per-process
        // value and returns the prior one.
        UmaskGuard(unsafe { libc::umask(0o177) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: see `owner_only`.
        unsafe {
            libc::umask(self.0);
        }
    }
}

/// Bind the control socket with owner-only (`0600`) permissions.
///
/// Security model: the daemon makes privileged DNS changes; the CLI does
/// not. The socket is the privilege boundary — any process that can write it
/// can change DNS. The socket is created `0600` *atomically* via a tightened
/// umask around `bind()` (no world-accessible window), restricting control to
/// the user running the daemon (for the system service, root). The containing
/// directory is also `0700` (`$XDG_RUNTIME_DIR` already is; the
/// `/run/splitway` fallback is created that way, or pre-created by systemd's
/// `RuntimeDirectory=`). For unprivileged multi-user control, an operator
/// would widen this to `0660` owned by a dedicated group — not done by
/// default, to avoid silently broadening who can change DNS.
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    // An existing socket file is either stale (from an unclean shutdown) or a
    // live daemon. Probe it before removing: unconditionally unlinking would
    // let a second `run` hijack the path from a running daemon, ending with
    // two daemons mutating DNS.
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "another splitway-daemon is already listening on {}",
                        path.display()
                    ),
                ));
            }
            // No listener: the socket is stale, safe to remove.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                log::warn!("removing stale socket at {}", path.display());
                std::fs::remove_file(path)?;
            }
            // It vanished between the check and the probe — nothing to remove.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Ambiguous (e.g. permissions): do not hijack a path we cannot
            // prove is dead.
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "cannot probe existing socket {}: {e}",
                    path.display()
                )));
            }
        }
    }
    // Create the socket 0600 atomically: the guard restores the prior umask
    // on drop, success or error.
    let listener = {
        let _umask = UmaskGuard::owner_only();
        UnixListener::bind(path)?
    };
    Ok(listener)
}

/// Accept connections forever, handling each in its own task. All requests
/// funnel into the one `state_tx`, so the state task still serializes them.
pub async fn serve(listener: UnixListener, state_tx: mpsc::Sender<StateCommand>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = state_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, tx).await {
                        log::debug!("IPC connection ended: {e}");
                    }
                });
            }
            Err(e) => {
                // A persistent accept error would otherwise spin this loop.
                log::error!("IPC accept failed: {e}");
                break;
            }
        }
    }
}

/// Cap a single request line. Requests are tiny (a domain at most), so this
/// is generous; it just stops a buggy or hostile client from making the
/// daemon buffer an unbounded line. Per-request (not per-connection) so
/// multiple requests can still share a connection.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

async fn handle_connection(
    stream: UnixStream,
    state_tx: mpsc::Sender<StateCommand>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = read_line_capped(&mut reader, &mut line, MAX_REQUEST_BYTES).await?;
        if n == 0 {
            break; // EOF
        }
        let response = match std::str::from_utf8(&line) {
            Ok(text) if text.trim().is_empty() => continue,
            Ok(text) => process_line(text, &state_tx).await,
            Err(_) => Response::Error("malformed request: not valid UTF-8".to_string()),
        };
        let mut encoded = serde_json::to_string(&response)
            .unwrap_or_else(|e| format!("{{\"Error\":\"failed to encode response: {e}\"}}"));
        encoded.push('\n');
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;
    }
    Ok(())
}

/// Read one `\n`-terminated line into `buf`, erroring if it would exceed
/// `max` bytes. Returns the number of bytes read (0 at EOF).
async fn read_line_capped(
    reader: &mut (impl AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len()); // EOF
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                return Ok(buf.len());
            }
            None => {
                buf.extend_from_slice(available);
                let consumed = available.len();
                reader.consume(consumed);
                if buf.len() > max {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "IPC request line exceeds maximum length",
                    ));
                }
            }
        }
    }
}

async fn process_line(line: &str, state_tx: &mpsc::Sender<StateCommand>) -> Response {
    let envelope: RequestEnvelope = match serde_json::from_str(line) {
        Ok(envelope) => envelope,
        Err(e) => {
            log::warn!("malformed IPC request: {e}");
            return Response::Error(format!("malformed request: {e}"));
        }
    };
    if envelope.version != PROTOCOL_VERSION {
        return Response::Error(format!(
            "protocol version mismatch: daemon speaks {PROTOCOL_VERSION}, client sent {}",
            envelope.version
        ));
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if state_tx
        .send(StateCommand::Ipc {
            request: envelope.request,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return Response::Error("daemon state task is not running".to_string());
    }
    match reply_rx.await {
        Ok(response) => response,
        Err(_) => Response::Error("daemon dropped the request".to_string()),
    }
}
