//! Thin OpenVPN management-interface plumbing for the VPN event stream.
//!
//! All real logic lives in the pure `parser`/`state` modules; this file is the
//! socket glue and is intentionally not unit-tested. It connects to the
//! management interface, optionally authenticates, arms `log on all` + `state
//! on`, samples the current `state`, and feeds parsed transitions + pushed DNS
//! into the same `tokio::sync::mpsc::Sender<VpnEvent>` contract the other
//! detectors use. Only read-only commands are sent (`state`, `log`); the
//! detector never sends `signal`/`hold`.
//!
//! Reconnect policy mirrors the macOS "transient read error → keep last state"
//! rule: a dropped/erroring management socket is **not** treated as VPN-down
//! (that would revert rules). The async loop reconnects with capped backoff and
//! re-samples; only an OpenVPN `EXITING`/`RECONNECTING` state emits `Down`.

use std::io::{BufRead, Read, Write};
use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::mpsc::Sender;

use splitway_shared::platform::{PlatformError, VpnEvent, VpnInfo};

use super::parser::{parse_push_reply_dns, parse_state_line, ManagementAddr};
use super::state::transition_for_state;
use crate::detector::linux::state::{Deduper, Transition};

/// Initial reconnect delay after a management socket error.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Cap on the reconnect delay so a long-down management interface is still
/// retried promptly once it returns.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A streamed session must last at least this long to count as "healthy" and
/// reset the backoff. A connect that succeeds but fails fast — a rejected
/// management password, or a socket accepted before OpenVPN is ready — stays
/// under this and keeps the backoff escalating toward [`MAX_BACKOFF`], instead
/// of pinning at a tight reconnect loop.
const MIN_HEALTHY_SESSION: Duration = Duration::from_secs(10);
/// Connect timeout for the one-shot `detect` probe (blocking).
const DETECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-read timeout for the one-shot `detect` probe (blocking).
const DETECT_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Overall budget for the one-shot `detect` probe.
const DETECT_DEADLINE: Duration = Duration::from_secs(3);

/// Drive the management watch until the receiver is dropped. Reconnects with
/// backoff on any connection error, never emitting `Down` for a socket problem
/// (only OpenVPN state does). The `Deduper` and last-seen pushed DNS persist
/// across reconnects, so re-sampling `CONNECTED` after a transient drop is
/// suppressed rather than re-emitted.
pub(super) async fn run(
    interface: String,
    addr: ManagementAddr,
    password: Option<String>,
    tx: Sender<VpnEvent>,
) -> Result<(), PlatformError> {
    let mut dedup = Deduper::default();
    let mut last_dns: Vec<String> = Vec::new();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if tx.is_closed() {
            return Ok(());
        }
        match connect_and_stream(
            &interface,
            &addr,
            password.as_deref(),
            &mut dedup,
            &mut last_dns,
            &mut backoff,
            &tx,
        )
        .await
        {
            // The receiver was dropped: stop watching.
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!(
                    "openvpn management ({addr}) error: {e}; reconnecting in {backoff:?} \
                     (VPN state left unchanged)"
                );
                tokio::select! {
                    _ = tx.closed() => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Connect to the management interface and stream until error or receiver drop.
/// The backoff is reset by [`run_session`] only after a session proves healthy,
/// not on mere connect — so a connect-then-fast-fail keeps escalating.
async fn connect_and_stream(
    interface: &str,
    addr: &ManagementAddr,
    password: Option<&str>,
    dedup: &mut Deduper,
    last_dns: &mut Vec<String>,
    backoff: &mut Duration,
    tx: &Sender<VpnEvent>,
) -> Result<(), PlatformError> {
    match addr {
        ManagementAddr::Tcp(endpoint) => {
            let stream = TcpStream::connect(endpoint).await?;
            log::debug!("connected to openvpn management at tcp {endpoint}");
            run_session(stream, interface, password, dedup, last_dns, backoff, tx).await
        }
        ManagementAddr::Unix(path) => {
            let stream = UnixStream::connect(path).await?;
            log::debug!("connected to openvpn management at unix {}", path.display());
            run_session(stream, interface, password, dedup, last_dns, backoff, tx).await
        }
    }
}

/// Stream a connected session, then reset `backoff` to its initial value only if
/// the session stayed up for at least [`MIN_HEALTHY_SESSION`]. A fast failure
/// (rejected password, not-yet-ready socket) leaves `backoff` to keep growing in
/// the caller, so repeated fast failures escalate toward [`MAX_BACKOFF`] rather
/// than spinning at a tight retry loop.
async fn run_session<S>(
    stream: S,
    interface: &str,
    password: Option<&str>,
    dedup: &mut Deduper,
    last_dns: &mut Vec<String>,
    backoff: &mut Duration,
    tx: &Sender<VpnEvent>,
) -> Result<(), PlatformError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started = Instant::now();
    let result = stream_session(stream, interface, password, dedup, last_dns, tx).await;
    if started.elapsed() >= MIN_HEALTHY_SESSION {
        *backoff = INITIAL_BACKOFF;
    }
    result
}

/// Authenticate (if a password is configured), arm notifications, sample the
/// current state, then forward every line until the socket closes or the
/// receiver is dropped. Generic over the stream so TCP and unix share one path.
async fn stream_session<S>(
    stream: S,
    interface: &str,
    password: Option<&str>,
    dedup: &mut Deduper,
    last_dns: &mut Vec<String>,
    tx: &Sender<VpnEvent>,
) -> Result<(), PlatformError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();

    // A password-protected management interface expects the password as the
    // very first line sent by the client.
    if let Some(password) = password {
        write_line(&mut write_half, password).await?;
    }

    // Arm real-time log + state BEFORE sampling current state, so a transition
    // racing setup is queued (and deduped against the sample) rather than lost.
    // `log on all` replays buffered log, letting an attach-after-connect recover
    // the pushed DNS from a historical PUSH_REPLY (if still in the log buffer).
    write_line(&mut write_half, "log on all").await?;
    write_line(&mut write_half, "state on").await?;
    write_line(&mut write_half, "state").await?;

    loop {
        tokio::select! {
            _ = tx.closed() => return Ok(()),
            read = lines.next_line() => {
                match read? {
                    Some(line) => handle_line(&line, interface, dedup, last_dns, tx).await,
                    // EOF: OpenVPN closed the socket. Not "VPN down" — let the
                    // caller reconnect with backoff.
                    None => {
                        return Err(PlatformError::CommandFailed(
                            "openvpn closed the management connection".to_string(),
                        ))
                    }
                }
            }
        }
    }
}

/// Process one management line: a `PUSH_REPLY` updates the pushed DNS held for
/// the next `Up`; a state line drives a deduplicated up/down event.
async fn handle_line(
    line: &str,
    interface: &str,
    dedup: &mut Deduper,
    last_dns: &mut Vec<String>,
    tx: &Sender<VpnEvent>,
) {
    if line.contains("PUSH_REPLY") {
        // Record the pushed DNS (possibly empty — the no-pushed-DNS case) so the
        // next `Up` carries it. Only a real PUSH_REPLY updates it, so unrelated
        // log lines never clear a known-good set.
        //
        // Known limitation (phase 3c uses the NM-style Up/Down `Deduper` the
        // prompt prescribed, which keys on the transition, not the server set):
        // a PUSH_REPLY that *changes* the servers while the tunnel stays
        // CONNECTED updates `last_dns` but emits no new `Up` (the dedup sees a
        // duplicate Up), so a mid-session DNS rotation on TLS renegotiation is
        // not re-applied until the next down/up. Rare in practice (a reneg
        // usually re-pushes the same servers); a DNS-server-aware re-emit (as
        // the macOS detector does) is a noted follow-up. See README.
        *last_dns = parse_push_reply_dns(line);
        return;
    }
    let Some(state) = parse_state_line(line) else {
        return;
    };
    let Some(transition) = transition_for_state(state) else {
        return; // intermediate/unknown state — no rule change
    };
    if dedup.push(transition).is_none() {
        return; // consecutive duplicate — already emitted
    }
    let event = match transition {
        Transition::Up => VpnEvent::Up(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers: last_dns.clone(),
        }),
        Transition::Down => {
            // A real OpenVPN down ends this session's DNS context, so drop the
            // cached pushed DNS: the next `Up` must not reuse the previous
            // session's servers. If the management socket reconnects only after
            // the new tunnel is already up and its `PUSH_REPLY` has aged out of
            // the bounded `log on all` replay (or the new session pushes no
            // DNS), the following `Up` then carries empty `dns_servers` (the
            // backend no-ops) instead of applying stale split-DNS rules. Only a
            // fresh `PUSH_REPLY` repopulates it. (A socket-level drop, by
            // contrast, emits no `Down` and keeps `last_dns` — see `run`.)
            last_dns.clear();
            VpnEvent::Down {
                interface_name: interface.to_string(),
            }
        }
    };
    send_event(tx, event).await;
}

async fn write_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    command: &str,
) -> Result<(), PlatformError> {
    writer.write_all(command.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn send_event(tx: &Sender<VpnEvent>, event: VpnEvent) {
    // A send error means the receiver was dropped; the `tx.closed()` branch
    // of the session loop stops the task right after.
    if let Err(e) = tx.send(event).await {
        log::debug!("openvpn VPN event receiver dropped: {e}");
    }
}

/// One-shot blocking probe for `OpenVpnDetector::detect`: connect, arm `log on
/// all` + `state`, read past the `log` replay to the `state` reply's `END` (or
/// a short deadline), and report whether the VPN is connected plus any pushed
/// DNS observed. Uses blocking std sockets so `detect` needs no async runtime,
/// mirroring the NM detector's synchronous `nmcli` call.
pub(super) fn blocking_sample(
    addr: &ManagementAddr,
    password: Option<&str>,
) -> Result<(bool, Vec<String>), PlatformError> {
    match addr {
        ManagementAddr::Tcp(endpoint) => {
            let socket_addr = endpoint
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| PlatformError::ParseError(format!("cannot resolve {endpoint}")))?;
            let stream = StdTcpStream::connect_timeout(&socket_addr, DETECT_CONNECT_TIMEOUT)?;
            stream.set_read_timeout(Some(DETECT_READ_TIMEOUT))?;
            blocking_session(stream, password)
        }
        ManagementAddr::Unix(path) => {
            let stream = StdUnixStream::connect(path)?;
            stream.set_read_timeout(Some(DETECT_READ_TIMEOUT))?;
            blocking_session(stream, password)
        }
    }
}

fn blocking_session<S: Read + Write>(
    mut stream: S,
    password: Option<&str>,
) -> Result<(bool, Vec<String>), PlatformError> {
    if let Some(password) = password {
        write_line_blocking(&mut stream, password)?;
    }
    // `log on all` replays the buffered log first (so an attach-after-connect
    // probe still recovers the historical `PUSH_REPLY`), then `state` reports
    // whether the tunnel is up. Both are multi-line command replies, and the
    // management interface terminates every multi-line reply with an `END` line
    // and answers commands strictly in order — so the stream carries exactly
    // two `END`s: the first ends the `log` replay, the second ends the `state`
    // reply. The `state` answer follows the first `END`, so we must read past
    // it and stop only at the second.
    //
    // We key on the `END` count, not on line content, by design: a replayed
    // history log line is emitted WITHOUT the real-time `>LOG:` prefix (bare
    // `<time>,<flags>,<msg>`), so it is structurally indistinguishable from a
    // `state` reply line and a "stop at the first state-looking line" check
    // would stop at the log replay. (Its `<flags>` field is never a
    // CONNECTED/EXITING/RECONNECTING token, so it cannot corrupt `connected`.)
    // The deadline / read-timeout below is the backstop if a reply is missing.
    write_line_blocking(&mut stream, "log on all")?;
    write_line_blocking(&mut stream, "state")?;

    // `log on all` then `state`: the `state` answer completes at the 2nd `END`.
    const END_TERMINATED_REPLIES: u32 = 2;

    let deadline = Instant::now() + DETECT_DEADLINE;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    let mut connected = false;
    let mut dns = Vec::new();
    let mut ends_seen: u32 = 0;

    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed == "END" {
                    ends_seen += 1;
                    if ends_seen >= END_TERMINATED_REPLIES {
                        break; // the `state` reply is complete
                    }
                    continue; // terminator of the `log` replay — keep reading
                }
                if trimmed.contains("PUSH_REPLY") {
                    let parsed = parse_push_reply_dns(trimmed);
                    if !parsed.is_empty() {
                        dns = parsed;
                    }
                } else if let Some(state) = parse_state_line(trimmed) {
                    match transition_for_state(state) {
                        Some(Transition::Up) => connected = true,
                        Some(Transition::Down) => connected = false,
                        None => {}
                    }
                }
            }
            // A read timeout means no more data is forthcoming right now: stop
            // reading and report what we have.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(e) => return Err(PlatformError::Io(e)),
        }
    }
    Ok((connected, dns))
}

fn write_line_blocking<W: Write>(writer: &mut W, command: &str) -> Result<(), PlatformError> {
    writer.write_all(command.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}
