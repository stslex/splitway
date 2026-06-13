//! Unix-domain-socket IPC server. Accepts newline-delimited JSON requests
//! and funnels each one into the single state-owner task via an `mpsc` +
//! `oneshot` reply channel. A malformed request is logged and answered with
//! an error response — never a panic.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use splitway_shared::ipc::{RequestEnvelope, Response, PROTOCOL_VERSION};

use crate::daemon::state::StateCommand;

/// Bind the control socket with owner-only (`0600`) permissions.
///
/// Security model: the daemon makes privileged DNS changes; the CLI does
/// not. The socket is the privilege boundary — any process that can write it
/// can change DNS. `0600` restricts that to the user running the daemon (for
/// the system service, root). The containing directory is `0700`
/// (`$XDG_RUNTIME_DIR` already is; the `/run/splitway` fallback is created
/// that way), which also covers the brief window between `bind` and the
/// `set_permissions` below. For unprivileged multi-user control, an operator
/// would widen this to `0660` owned by a dedicated group — not done by
/// default, to avoid silently broadening who can change DNS.
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    // A socket left over from an unclean shutdown would make bind fail with
    // EADDRINUSE. We are the single daemon instance, so removing it is safe.
    if path.exists() {
        std::fs::remove_file(path)?;
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
                // A persistent accept error would otherwise spin this loop.
                log::error!("IPC accept failed: {e}");
                break;
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    state_tx: mpsc::Sender<StateCommand>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = process_line(&line, &state_tx).await;
        let mut encoded = serde_json::to_string(&response)
            .unwrap_or_else(|e| format!("{{\"Error\":\"failed to encode response: {e}\"}}"));
        encoded.push('\n');
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;
    }
    Ok(())
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
