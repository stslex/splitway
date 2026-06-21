//! Unix-domain-socket IPC server. Accepts newline-delimited JSON requests
//! and funnels each one into the single state-owner task via an `mpsc` +
//! `oneshot` reply channel. A malformed request is logged and answered with
//! an error response — never a panic.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use splitway_shared::ipc::{RequestEnvelope, Response, PROTOCOL_VERSION, VERSION_MISMATCH_PREFIX};

use crate::daemon::state::StateCommand;

/// Bind the control socket. By default it is owner-only (`0600`, root for the
/// system service); with `socket_group` set it is `0660` owned by that group,
/// inside a `0750 root:<group>` runtime dir.
///
/// Security model: the daemon makes privileged DNS changes; the CLI/GUI do not.
/// The socket is the privilege boundary — any process that can write it can
/// change DNS. With no group the socket is `chmod`ed to `0600`, restricting
/// control to the user running the daemon (root, for the system service). The
/// containing directory is required to be `0700` first (the system-service
/// socket dir is created/tightened to it; `$XDG_RUNTIME_DIR` is verified
/// user-private and the bind refused otherwise), so the brief window between
/// `bind()` and the `chmod` is not reachable by other users. (The system socket
/// dir is `SYSTEM_SOCKET_DIR` from `splitway-shared`: `/run/splitway` on Linux,
/// `/var/run/splitway` on macOS.)
///
/// # Socket group (opt-in, `--socket-group`)
///
/// When `socket_group` names a group, the runtime dir becomes `0750 root:<group>`
/// and the socket `0660 root:<group>` — defense in depth: the dir gate means a
/// non-member cannot even *traverse* to probe the socket path, and the socket
/// mode lets members connect. This is how an unprivileged in-group GUI/CLI reaches
/// a root daemon without `sudo` (the prerequisite for the niri/Tauri GUI, which
/// runs as a normal user with no system tray).
///
/// **Security note:** membership in this group grants the ability to drive the
/// daemon's privileged split-DNS operations (`resolvectl`/`nmcli`). Adding a user
/// to the group ≈ granting them control of system split-DNS routing. That is why
/// it is opt-in and the group is empty by default. (Stronger per-peer auth via
/// `SO_PEERCRED` is a later phase.)
///
/// Failure handling is **fail-fast**: an unresolvable group name, or a `chown`
/// the daemon is not privileged to perform (not root, and not in the group),
/// aborts the bind — the daemon then exits non-zero — rather than silently
/// running with ambiguous permissions, which is the worse outcome.
///
/// `umask` is avoided deliberately (it is process-global and would race file
/// creation in other tasks of this multi-threaded daemon). The `bind()`→`chmod`
/// window is instead closed by ordering: the socket is `chmod`ed to its final
/// mode while still owned by `root:root` (so only root can connect), and *then*
/// `chgrp`ed to `<group>` — at which instant it is already `0660`, so exactly the
/// target group gains access and never a wider set. The `0750` parent dir,
/// tightened before the socket is bound, blocks every non-member regardless.
pub fn bind_socket(path: &Path, socket_group: Option<&str>) -> std::io::Result<UnixListener> {
    // Resolve the group up front: a bad name must fail before we touch the
    // filesystem (and before unlinking any stale socket), so a misconfigured
    // --socket-group never disturbs an already-bound socket or runtime dir.
    let gid = match socket_group {
        Some(name) => Some(resolve_socket_group(name)?),
        None => None,
    };
    bind_socket_with_gid(path, gid)
}

/// The filesystem half of [`bind_socket`], parameterized by an already-resolved
/// gid so the group lookup is injected in tests (no real dedicated group or root
/// is needed: the tests pass the caller's own primary gid, which `chgrp` always
/// permits unprivileged).
fn bind_socket_with_gid(path: &Path, gid: Option<libc::gid_t>) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        // The socket's own mode is only applied after bind(), so a tightened
        // parent is what closes that window. A failure here is fatal (propagated),
        // since the security model depends on it.
        let is_xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .is_some_and(|value| Path::new(&value) == dir);
        if is_xdg_runtime {
            // The shared session dir is the user's own (already 0700, user-owned);
            // we neither chmod nor chgrp it even when a group is requested. The
            // group socket is a system-service feature — the system dir below
            // carries the group gate — and a 0660 socket here is only reachable if
            // the session dir grants group traversal, which a user-private
            // $XDG_RUNTIME_DIR does not. We still verify it is user-private so the
            // bind()->chmod window is not exposed.
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
            // A socket group only makes sense for the system-service deployment.
            // Here the socket falls in a user-private $XDG_RUNTIME_DIR whose 0700
            // mode blocks group traversal regardless of the socket's own mode, so
            // the request is inert. We do not silently pretend it took effect:
            // surface it loudly. (It fails closed — the dir denies non-members —
            // so this is a deployment mismatch, not a hard error: a user-session
            // daemon is simply not where the group socket belongs.)
            if gid.is_some() {
                log::warn!(
                    "--socket-group is ineffective here: the control socket resolves \
                     under the user-private $XDG_RUNTIME_DIR ({}), whose 0700 mode \
                     blocks group members from reaching it. The socket group is a \
                     system-service feature — run the daemon so the socket falls back \
                     to the system runtime dir (under systemd with RuntimeDirectory, \
                     i.e. without XDG_RUNTIME_DIR set).",
                    dir.display()
                );
            }
        } else {
            // The system-service socket dir is ours: create it and apply the
            // ownership/mode the group choice calls for — 0700 root, or
            // 0750 root:<group>. Idempotent: if systemd's RuntimeDirectory
            // pre-created it (e.g. root:root 0755), this tightens it to the same
            // end state. Done before bind() so the socket is never reachable by a
            // non-member during the bind()->chmod window.
            std::fs::create_dir_all(dir)?;
            apply_group_perms(dir, gid, 0o700, 0o750)?;
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
    // 0600 root (default), or 0660 root:<group>. chmod-then-chgrp: see the window
    // discussion on `bind_socket`.
    apply_group_perms(path, gid, 0o600, 0o660)?;
    Ok(listener)
}

/// `chmod` `path` to `with_group` when a socket group is set (then `chgrp` it to
/// that gid), else `without_group`. The chmod-before-chgrp order closes the
/// ownership window: the path reaches its final, narrower mode while still owned
/// by `root:root`, and only then is handed to the group.
fn apply_group_perms(
    path: &Path,
    gid: Option<libc::gid_t>,
    without_group: u32,
    with_group: u32,
) -> std::io::Result<()> {
    let mode = if gid.is_some() {
        with_group
    } else {
        without_group
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    if let Some(gid) = gid {
        chgrp(path, gid).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "failed to set group ownership of {} (gid {gid}); the daemon \
                     must run as root or be a member of the group: {e}",
                    path.display()
                ),
            )
        })?;
    }
    Ok(())
}

/// `chgrp` `path` to `gid`, leaving the owning uid unchanged, via `libc::chown`
/// (no subprocess). `(uid_t)-1` is the POSIX "do not change the owner" sentinel.
fn chgrp(path: &Path, gid: libc::gid_t) -> std::io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket path contains an interior NUL byte",
        )
    })?;
    // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the call;
    // chown reads it and does not retain the pointer.
    let rc = unsafe { libc::chown(c_path.as_ptr(), libc::uid_t::MAX, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Resolve a socket-group name to its gid, failing fast with an actionable
/// message when the group does not exist (the caller aborts the bind and the
/// daemon exits non-zero).
fn resolve_socket_group(name: &str) -> std::io::Result<libc::gid_t> {
    match group_gid(name)? {
        Some(gid) => Ok(gid),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "socket group `{name}` not found; enable \
                 services.splitway.unprivilegedGui (the NixOS module creates the \
                 group), or create it manually (e.g. `groupadd {name}`) before \
                 passing --socket-group"
            ),
        )),
    }
}

/// Look up a group's gid by name via `getgrnam_r` (no subprocess). `Ok(None)`
/// means the group does not exist; `Err` is a real lookup failure.
fn group_gid(name: &str) -> std::io::Result<Option<libc::gid_t>> {
    let c_name = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "group name contains an interior NUL byte",
        )
    })?;
    // getgrnam_r writes into a caller-provided buffer; start at the suggested
    // size and grow on ERANGE. rc 0 with a NULL result pointer means "no such
    // group" (distinct from an error).
    let mut buf_len = match unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) } {
        len if len > 0 => len as usize,
        _ => 1024,
    };
    loop {
        let mut group: libc::group = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; buf_len];
        let mut result: *mut libc::group = std::ptr::null_mut();
        // SAFETY: every pointer references live, correctly-sized local storage for
        // the duration of the call; getgrnam_r writes only within `group`/`buf`
        // and sets `result` to either `&group` or NULL.
        let rc = unsafe {
            libc::getgrnam_r(
                c_name.as_ptr(),
                &mut group,
                buf.as_mut_ptr(),
                buf_len,
                &mut result,
            )
        };
        if rc == 0 {
            return Ok((!result.is_null()).then_some(group.gr_gid));
        }
        if rc == libc::ERANGE {
            buf_len = buf_len
                .checked_mul(2)
                .ok_or_else(|| std::io::Error::other("group lookup buffer too large"))?;
            continue;
        }
        return Err(std::io::Error::from_raw_os_error(rc));
    }
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
    // Serializing a `Response` does not fail in practice, but if it ever did,
    // build the fallback through the serializer too so the error text is
    // JSON-escaped and the client still receives parseable JSON — a hand-built
    // string could embed a raw quote or control character and desync the
    // client. The final arm is a fixed, already-valid JSON literal.
    let mut encoded = serde_json::to_string(response).unwrap_or_else(|e| {
        serde_json::to_string(&Response::Error(format!("failed to encode response: {e}")))
            .unwrap_or_else(|_| r#"{"Error":"failed to encode response"}"#.to_string())
    });
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

/// The protocol version alone, decoded without the `request` body. Unknown
/// fields (including `request`) are ignored, so this parses regardless of which
/// request variants the peer knows — letting a version check run *before* the
/// request body is deserialized.
#[derive(serde::Deserialize)]
struct VersionPeek {
    version: u32,
}

async fn process_line(line: &str, state_tx: &mpsc::Sender<StateCommand>) -> Response {
    // Check the version first, from a peek that ignores the `request` body. A
    // client newer than this daemon may send a request variant the daemon does
    // not know; deserializing the full envelope first would fail that as a raw
    // "unknown variant" decode error rather than the actionable version
    // mismatch below. Peeking the version keeps skew reported as "update
    // splitway", never a decode error.
    match serde_json::from_str::<VersionPeek>(line) {
        Ok(peek) if peek.version != PROTOCOL_VERSION => {
            return Response::Error(format!(
                "{VERSION_MISMATCH_PREFIX}: daemon speaks {PROTOCOL_VERSION}, \
                 client speaks {} — update splitway so the daemon and client run \
                 the same protocol version",
                peek.version
            ));
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("malformed IPC request: {e}");
            return Response::Error(format!("malformed request: {e}"));
        }
    }

    let envelope: RequestEnvelope = match serde_json::from_str(line) {
        Ok(envelope) => envelope,
        Err(e) => {
            log::warn!("malformed IPC request: {e}");
            return Response::Error(format!("malformed request: {e}"));
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn version_mismatch_is_reported_before_request_parse() {
        let (tx, _rx) = mpsc::channel::<StateCommand>(1);
        // A wrong version carrying a request variant this daemon does not know:
        // the version is checked from a peek first, so this is reported as a
        // version mismatch ("update splitway"), never a raw decode error.
        let line = r#"{"version":999,"request":{"FutureVerb":{}}}"#;
        match process_line(line, &tx).await {
            Response::Error(msg) => {
                assert!(
                    msg.starts_with(VERSION_MISMATCH_PREFIX),
                    "expected a version-mismatch error, got: {msg}"
                );
            }
            other => panic!("expected a version-mismatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_as_error() {
        let (tx, _rx) = mpsc::channel::<StateCommand>(1);
        assert!(matches!(
            process_line("this is not json", &tx).await,
            Response::Error(_)
        ));
    }

    use std::os::unix::fs::MetadataExt;

    /// A unique, test-owned temp directory to host a bound socket without
    /// touching `/run` or `$XDG_RUNTIME_DIR` (whose perms `bind_socket` manages).
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "splitway-ipc-test-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn group_gid_returns_none_for_a_missing_group() {
        // A name no system would define: resolution reports "not found" (Ok(None)),
        // not an error, so the caller can produce its own actionable message.
        assert_eq!(
            group_gid("splitway-no-such-group-zzz").unwrap(),
            None,
            "a non-existent group must resolve to None, not error"
        );
    }

    #[test]
    fn resolve_socket_group_fails_fast_with_an_actionable_message() {
        let err = resolve_socket_group("splitway-no-such-group-zzz").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(msg.contains("splitway-no-such-group-zzz"), "got: {msg}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[tokio::test]
    async fn bind_without_group_is_owner_only_0600() {
        let dir = unique_temp_dir("nogroup");
        let sock = dir.join("splitway.sock");

        let _listener = bind_socket_with_gid(&sock, None).unwrap();

        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "socket is owner-only by default");
        // The dir we created is tightened to 0700 (no group requested).
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777;
        assert_eq!(dir_mode, 0o700, "runtime dir is owner-only by default");
        // The owner can still connect.
        std::os::unix::net::UnixStream::connect(&sock).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bind_with_group_is_0660_owned_by_that_gid() {
        // Inject the caller's own primary group as the "socket group": `chgrp`
        // to a group you belong to always succeeds unprivileged, so this proves
        // the mode + ownership end state without a real dedicated group or root.
        // (Cross-user *denial* is proven by the nixosTest in nix/tests/.)
        let gid = unsafe { libc::getgid() };
        let dir = unique_temp_dir("group");
        let sock = dir.join("splitway.sock");

        let _listener = bind_socket_with_gid(&sock, Some(gid)).unwrap();

        let sock_meta = std::fs::metadata(&sock).unwrap();
        assert_eq!(
            sock_meta.permissions().mode() & 0o7777,
            0o660,
            "socket is group-rw with a socket group"
        );
        assert_eq!(sock_meta.gid(), gid, "socket is owned by the socket group");

        let dir_meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(
            dir_meta.permissions().mode() & 0o7777,
            0o750,
            "runtime dir gates traversal by the socket group"
        );
        assert_eq!(
            dir_meta.gid(),
            gid,
            "runtime dir is owned by the socket group"
        );

        std::os::unix::net::UnixStream::connect(&sock).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }
}
