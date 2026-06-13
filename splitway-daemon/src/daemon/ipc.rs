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

/// Bind the control socket with owner-only (`0600`) permissions.
///
/// Security model: the daemon makes privileged DNS changes; the CLI does
/// not. The socket is the privilege boundary — any process that can write it
/// can change DNS. The socket is created, then `chmod`ed to `0600`,
/// restricting control to the user running the daemon (for the system
/// service, root). The containing directory is required to be `0700` first
/// (the system-service socket dir fallback is created and chmod'd;
/// `$XDG_RUNTIME_DIR` is verified user-private and the bind refused
/// otherwise), so the brief window
/// between `bind()` and the `chmod` is not reachable by other users.
/// (The system socket dir is `SYSTEM_SOCKET_DIR` from `splitway-shared`:
/// `/run/splitway` on Linux, `/var/run/splitway` on macOS.)
/// (`umask` is avoided deliberately: it is process-global and would
/// race file creation in other tasks of this multi-threaded daemon.) For
/// unprivileged multi-user control, an operator would widen this to `0660`
/// owned by a dedicated group — not done by default, to avoid silently
/// broadening who can change DNS.
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        // The socket's own mode is only applied after bind(), so a 0700 parent
        // is what closes that window. A failure here is fatal (propagated),
        // since the security model depends on the 0700 parent.
        let is_xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .is_some_and(|value| Path::new(&value) == dir);
        if is_xdg_runtime {
            // We do not chmod the shared session dir, but we must not trust it
            // blindly either: if $XDG_RUNTIME_DIR is misconfigured to be
            // group/other-accessible, the bind()->chmod window below would be
            // exposed. Verify it is user-private and fail fast otherwise.
            let mode = std::fs::metadata(dir)?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to bind: {} is not user-private (mode {:o}); \
                         expected no group/other access",
                        dir.display(),
                        mode & 0o7777
                    ),
                ));
            }
        } else {
            // The system-service socket dir fallback is ours: create + 0700.
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
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
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
                // Most accept() errors are transient: a client that hangs up
                // between connect and accept yields ECONNABORTED; fd pressure
                // yields EMFILE/ENFILE. Keep serving rather than tearing down
                // the control socket for the rest of the daemon's lifetime; a
                // short backoff avoids a busy-spin if the condition persists.
                log::warn!("IPC accept failed (continuing): {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Cap a single request line. Requests are tiny (a domain at most), so this
/// is generous; it just stops a buggy or hostile client from making the
/// daemon buffer an unbounded line. Per-request (not per-connection) so
/// multiple requests can still share a connection.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Outcome of reading one request line.
enum LineRead {
    /// A complete line is available in the caller's buffer.
    Line,
    /// Clean EOF with no pending line.
    Eof,
    /// The line exceeded [`MAX_REQUEST_BYTES`] before a newline was seen.
    TooLong,
}

async fn handle_connection(
    stream: UnixStream,
    state_tx: mpsc::Sender<StateCommand>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();
    loop {
        line.clear();
        let response = match read_line_capped(&mut reader, &mut line, MAX_REQUEST_BYTES).await? {
            LineRead::Eof => break,
            LineRead::TooLong => {
                // The stream is desynced past the cap; answer with an error
                // (per the documented contract — never silently drop) and
                // close the connection rather than try to resynchronize.
                log::warn!("IPC request exceeded {MAX_REQUEST_BYTES} bytes; rejecting");
                write_response(
                    &mut write_half,
                    &Response::Error("request exceeds maximum length".to_string()),
                )
                .await?;
                break;
            }
            LineRead::Line => match std::str::from_utf8(&line) {
                Ok(text) if text.trim().is_empty() => continue,
                Ok(text) => process_line(text, &state_tx).await,
                Err(_) => Response::Error("malformed request: not valid UTF-8".to_string()),
            },
        };
        write_response(&mut write_half, &response).await?;
    }
    Ok(())
}

async fn write_response(
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
    response: &Response,
) -> std::io::Result<()> {
    let mut encoded = serde_json::to_string(response)
        .unwrap_or_else(|e| format!("{{\"Error\":\"failed to encode response: {e}\"}}"));
    encoded.push('\n');
    write_half.write_all(encoded.as_bytes()).await?;
    write_half.flush().await
}

/// Read one `\n`-terminated line into `buf`, enforcing the `max` cap whether
/// or not the terminating newline lands in the same `fill_buf` chunk.
async fn read_line_capped(
    reader: &mut (impl AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<LineRead> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if buf.len() + i + 1 > max {
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                return Ok(LineRead::Line);
            }
            None => {
                buf.extend_from_slice(available);
                let consumed = available.len();
                reader.consume(consumed);
                if buf.len() > max {
                    return Ok(LineRead::TooLong);
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
