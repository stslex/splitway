//! Pure parsing of the OpenVPN management interface — no I/O, unit tested.
//!
//! The management protocol multiplexes several line kinds on one connection
//! (`man openvpn`, "MANAGEMENT INTERFACE"). The three this detector cares about:
//!
//! - real-time state notifications, `>STATE:<time>,<state>,<desc>,<localip>,...`,
//!   and the bare `<time>,<state>,...` form the `state` command replies with;
//! - the pushed-DNS line surfaced by `log on`, a `>LOG:<time>,<flags>,PUSH:
//!   Received control message: 'PUSH_REPLY,...,dhcp-option DNS <ip>,...'`. Note
//!   the `log on all` *history replay* (used for attach-after-connect recovery)
//!   emits the same content WITHOUT the real-time `>LOG:` prefix — a bare
//!   `<time>,<flags>,PUSH: ...` line — so the DNS parser keys on `PUSH_REPLY`
//!   and the option structure, never on the prefix;
//! - the management address from config, either `host:port` (TCP) or a unix
//!   socket path.
//!
//! Fixtures below are reconstructed from the documented protocol; confirm the
//! exact byte layout against a live capture (see the PR's investigation notes).

use std::path::PathBuf;

use splitway_shared::platform::PlatformError;

/// Where the OpenVPN management interface is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManagementAddr {
    /// A `host:port` TCP endpoint (`management 127.0.0.1 7505`).
    Tcp(String),
    /// A unix socket path (`management /run/openvpn/mgmt.sock unix`).
    Unix(PathBuf),
}

impl std::fmt::Display for ManagementAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagementAddr::Tcp(addr) => write!(f, "tcp {addr}"),
            ManagementAddr::Unix(path) => write!(f, "unix {}", path.display()),
        }
    }
}

/// Interpret the config `management` string: a value containing `/` is a unix
/// socket path, anything else a `host:port` TCP endpoint. An empty value is an
/// error (the detector cannot connect without an address).
pub(crate) fn parse_management_addr(raw: &str) -> Result<ManagementAddr, PlatformError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PlatformError::ParseError(
            "openvpn.management is empty; set it to \"host:port\" or a unix socket path"
                .to_string(),
        ));
    }
    if trimmed.contains('/') {
        Ok(ManagementAddr::Unix(PathBuf::from(trimmed)))
    } else {
        Ok(ManagementAddr::Tcp(trimmed.to_string()))
    }
}

/// Extract the OpenVPN state token from a management state line, e.g.
/// `>STATE:1700000000,CONNECTED,SUCCESS,10.8.0.2,...` or the bare
/// `1700000000,CONNECTED,SUCCESS,...` form a `state` query replies with, both
/// yielding `"CONNECTED"`. Returns `None` for any other line (the stream is
/// multiplexed with log, command-reply and info lines).
///
/// The state form is recognized structurally — a unix timestamp, then an
/// uppercase state token — so a comma-bearing log line is not misread as state.
pub(crate) fn parse_state_line(line: &str) -> Option<&str> {
    parse_state_line_with_time(line).map(|(_, state)| state)
}

/// Like [`parse_state_line`], but also returns the leading unix timestamp — the
/// time OpenVPN entered the state. For `CONNECTED` this is the tunnel's connect
/// time: stable across a transient management-socket reconnect over the *same*
/// tunnel, and renewed when the tunnel actually restarts. The watcher uses it to
/// tell a same-tunnel reconnect (keep cached DNS) from a genuinely new session
/// (do not reuse the previous session's pushed DNS).
pub(crate) fn parse_state_line_with_time(line: &str) -> Option<(u64, &str)> {
    let rest = line.strip_prefix(">STATE:").unwrap_or(line);
    let (time, after) = rest.split_once(',')?;
    if time.is_empty() || !time.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let state = after.split(',').next()?;
    // State tokens are uppercase ASCII words: CONNECTED, RECONNECTING, AUTH, ...
    if state.is_empty() || !state.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        return None;
    }
    // An all-digit field that overflows u64 is not a real management timestamp.
    let timestamp = time.parse::<u64>().ok()?;
    Some((timestamp, state))
}

/// Collect the pushed DNS servers from a management log line carrying a
/// `PUSH_REPLY`. Every `dhcp-option DNS <ip>` / `dhcp-option DNS6 <ip>` value is
/// collected in order (IPv4 and IPv6); all other pushed options are ignored.
/// Returns an empty vec when the line is not a `PUSH_REPLY` or carries no DNS
/// option (the no-pushed-DNS case the caller handles explicitly).
pub(crate) fn parse_push_reply_dns(line: &str) -> Vec<String> {
    if !line.contains("PUSH_REPLY") {
        return Vec::new();
    }
    // Pushed options are comma-separated inside the single-quoted control
    // message; an IP value never contains a comma, so splitting on ',' is safe.
    line.split(',')
        .filter_map(|option| {
            let option = option.trim().trim_end_matches('\'').trim();
            let ip = option
                .strip_prefix("dhcp-option DNS6 ")
                .or_else(|| option.strip_prefix("dhcp-option DNS "))?
                .trim()
                .trim_end_matches('\'')
                .trim();
            (!ip.is_empty()).then(|| ip.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_addr_tcp_vs_unix() {
        assert_eq!(
            parse_management_addr("127.0.0.1:7505").unwrap(),
            ManagementAddr::Tcp("127.0.0.1:7505".to_string())
        );
        assert_eq!(
            parse_management_addr("  localhost:7505  ").unwrap(),
            ManagementAddr::Tcp("localhost:7505".to_string())
        );
        assert_eq!(
            parse_management_addr("/run/openvpn/mgmt.sock").unwrap(),
            ManagementAddr::Unix(PathBuf::from("/run/openvpn/mgmt.sock"))
        );
        assert!(matches!(
            parse_management_addr("   "),
            Err(PlatformError::ParseError(_))
        ));
    }

    #[test]
    fn state_line_realtime_and_reply_forms() {
        // Real-time notification (>STATE: prefix).
        assert_eq!(
            parse_state_line(">STATE:1700000000,CONNECTED,SUCCESS,10.8.0.2,192.0.2.10,1194,,"),
            Some("CONNECTED")
        );
        // Bare reply form from the `state` command.
        assert_eq!(
            parse_state_line("1700000000,CONNECTED,SUCCESS,10.8.0.2,192.0.2.10,1194,,"),
            Some("CONNECTED")
        );
        assert_eq!(
            parse_state_line(">STATE:1700000123,RECONNECTING,ping-restart,,,,,"),
            Some("RECONNECTING")
        );
        assert_eq!(
            parse_state_line(">STATE:1700000200,EXITING,exit-with-notification,,,,,"),
            Some("EXITING")
        );
    }

    #[test]
    fn state_line_with_time_returns_timestamp_and_token() {
        assert_eq!(
            parse_state_line_with_time(">STATE:1700000000,CONNECTED,SUCCESS,10.8.0.2,192.0.2.10"),
            Some((1700000000, "CONNECTED"))
        );
        assert_eq!(
            parse_state_line_with_time("1700009999,CONNECTED,SUCCESS,10.9.0.2,192.0.2.11"),
            Some((1700009999, "CONNECTED"))
        );
        // Non-state lines carry no timestamp/state pair.
        assert_eq!(parse_state_line_with_time(">LOG:1,I,PUSH: ..."), None);
        assert_eq!(parse_state_line_with_time("END"), None);
    }

    #[test]
    fn intermediate_states_are_parsed_as_their_token() {
        // The parser only extracts the token; mapping/ignoring is state.rs's job.
        for (line, token) in [
            (">STATE:1,CONNECTING,,,,,,", "CONNECTING"),
            (">STATE:2,WAIT,,,,,,", "WAIT"),
            (">STATE:3,AUTH,,,,,,", "AUTH"),
            (">STATE:4,GET_CONFIG,,,,,,", "GET_CONFIG"),
            (">STATE:5,ASSIGN_IP,,10.8.0.2,,,,", "ASSIGN_IP"),
            (">STATE:6,ADD_ROUTES,,,,,,", "ADD_ROUTES"),
            (">STATE:7,RESOLVE,,,,,,", "RESOLVE"),
        ] {
            assert_eq!(parse_state_line(line), Some(token), "for {line}");
        }
    }

    #[test]
    fn non_state_lines_are_rejected() {
        // Greeting, command replies, log lines, and END must not parse as state.
        assert_eq!(
            parse_state_line(">INFO:OpenVPN Management Interface Version 5"),
            None
        );
        assert_eq!(
            parse_state_line("SUCCESS: real-time state notification set to ON"),
            None
        );
        assert_eq!(parse_state_line("END"), None);
        assert_eq!(
            parse_state_line(">LOG:1700000000,I,PUSH: Received control message"),
            None
        );
        // A timestamp with a lowercase second field is not a state token.
        assert_eq!(parse_state_line("1700000000,some message,extra"), None);
        assert_eq!(parse_state_line(""), None);
    }

    /// A representative `PUSH_REPLY` as surfaced through the management `log on`
    /// channel: a `>LOG:` line whose message embeds the single-quoted control
    /// message. Trimmed of options the parser ignores would still parse; the
    /// realistic mix here exercises the option filtering.
    const PUSH_REPLY_LOG: &str = ">LOG:1700000000,I,PUSH: Received control message: 'PUSH_REPLY,redirect-gateway def1,route-gateway 10.8.0.1,topology subnet,ping 10,ping-restart 120,dhcp-option DNS 10.8.0.1,dhcp-option DNS 10.8.0.2,dhcp-option DOMAIN corp.example.com,ifconfig 10.8.0.2 255.255.255.0,peer-id 0,cipher AES-256-GCM'";

    #[test]
    fn push_reply_collects_ipv4_dns_in_order() {
        assert_eq!(
            parse_push_reply_dns(PUSH_REPLY_LOG),
            vec!["10.8.0.1".to_string(), "10.8.0.2".to_string()]
        );
    }

    #[test]
    fn push_reply_collects_ipv6_via_dns_and_dns6() {
        // OpenVPN may carry a v6 address on `dhcp-option DNS` (newer) or the
        // legacy `dhcp-option DNS6`; both are collected, in order, with v4.
        let line = ">LOG:1700000000,I,PUSH: Received control message: 'PUSH_REPLY,dhcp-option DNS 10.8.0.1,dhcp-option DNS6 2001:db8::1,dhcp-option DNS fd00::2,route 10.8.0.0'";
        assert_eq!(
            parse_push_reply_dns(line),
            vec![
                "10.8.0.1".to_string(),
                "2001:db8::1".to_string(),
                "fd00::2".to_string(),
            ]
        );
    }

    #[test]
    fn push_reply_without_dns_is_empty() {
        // The no-pushed-DNS case: a PUSH_REPLY carrying no dhcp-option DNS.
        let line = ">LOG:1700000000,I,PUSH: Received control message: 'PUSH_REPLY,redirect-gateway def1,route 10.8.0.0,topology subnet,ping 10'";
        assert!(parse_push_reply_dns(line).is_empty());
    }

    #[test]
    fn push_reply_parses_prefixless_history_replay_form() {
        // The `log on all` history replay (attach-after-connect DNS recovery)
        // emits the PUSH line WITHOUT the real-time `>LOG:` prefix, as a bare
        // `<time>,<flags>,PUSH: ...`. The parser keys on `PUSH_REPLY`, not the
        // prefix, so the same DNS is recovered from the replayed form.
        let line = "1700000000,I,PUSH: Received control message: 'PUSH_REPLY,redirect-gateway def1,dhcp-option DNS 10.8.0.1,dhcp-option DNS6 2001:db8::1,route 10.8.0.0'";
        assert_eq!(
            parse_push_reply_dns(line),
            vec!["10.8.0.1".to_string(), "2001:db8::1".to_string()]
        );
    }

    #[test]
    fn non_push_reply_line_yields_no_dns() {
        // A stray `dhcp-option DNS` outside a PUSH_REPLY is ignored.
        assert!(parse_push_reply_dns(">LOG:1,I,something dhcp-option DNS 1.2.3.4").is_empty());
        assert!(parse_push_reply_dns("END").is_empty());
    }
}
