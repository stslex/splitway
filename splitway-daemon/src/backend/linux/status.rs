//! Pure parser for `resolvectl status <iface>` output, kept I/O-free so it is
//! unit-tested with synthetic fixtures. The backend shells out and hands the
//! captured stdout here.
//!
//! Observed format (systemd-resolved), one per-link block:
//!
//! ```text
//! Link 5 (tun0)
//!     Current Scopes: DNS
//!          Protocols: +DefaultRoute +LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
//! Current DNS Server: 10.0.0.1
//!        DNS Servers: 10.0.0.1 10.0.0.2
//!                     10.0.0.3
//!         DNS Domain: example.com ~corp.example.com
//!                     ~sub.example.com
//! ```
//!
//! Labels are right-aligned, so each field's value follows a `<Label>:` prefix.
//! A multi-valued field (`DNS Servers` / `DNS Domain`) may **wrap** onto
//! continuation lines that carry only values aligned under the first one — no
//! label. We collect those by tracking which field we are inside until the next
//! recognized field label (or a blank line) ends it.
//!
//! Defensive, mirroring `query.rs`: server tokens are kept only if they parse as
//! an IP address, so a header or stray label can never slip in as a "server".
//! Domains are collected as whitespace tokens, each with a leading `~`
//! (routing-only marker) stripped to the plain domain. Never fails — an empty
//! result means nothing parsed, which the caller treats as "read-back empty".

use splitway_shared::ipc::{server_address, LinkDnsState};

/// The multi-valued field a continuation line would extend, tracked so a wrapped
/// `DNS Servers` / `DNS Domain` list is gathered across lines.
enum Section {
    /// Not inside a multi-valued field (or a single-valued one like
    /// `Current DNS Server`); a continuation line here is ignored.
    Other,
    Servers,
    Domains,
}

/// Parse the per-link DNS servers and routing domains from `resolvectl status
/// <iface>` stdout. Never fails — an empty [`LinkDnsState`] means nothing parsed.
pub(crate) fn parse_resolvectl_status(output: &str) -> LinkDnsState {
    let mut servers: Vec<String> = Vec::new();
    let mut routing_domains: Vec<String> = Vec::new();
    let mut default_route: Option<bool> = None;
    // Fallback source for the default-route flag: older systemd-resolved output
    // exposes the catch-all only as the `Protocols: +DefaultRoute` token, with no
    // separate `Default Route:` line. The explicit line wins when both are present.
    let mut protocols_default_route: Option<bool> = None;
    let mut section = Section::Other;

    for line in output.lines() {
        let trimmed = line.trim();
        // A blank line separates blocks/fields — never the middle of a wrapped
        // value, so it ends any open continuation section.
        if trimmed.is_empty() {
            section = Section::Other;
            continue;
        }

        if let Some(values) = trimmed.strip_prefix("DNS Servers:") {
            section = Section::Servers;
            push_servers(values, &mut servers);
        } else if let Some(values) = trimmed.strip_prefix("Current DNS Server:") {
            // A single-valued field: collect its value, but do not open a
            // continuation section — the next line is a different field.
            section = Section::Other;
            push_servers(values, &mut servers);
        } else if let Some(values) = trimmed.strip_prefix("DNS Domain:") {
            section = Section::Domains;
            push_domains(values, &mut routing_domains);
        } else if let Some(value) = trimmed.strip_prefix("Default Route:") {
            // The per-link DNS default-route (catch-all) flag. Single-valued, so
            // it ends any continuation section. `yes`/`no` map to `Some`; anything
            // else leaves it unknown (`None`). This explicit line is authoritative
            // over the `Protocols: +DefaultRoute` fallback below.
            section = Section::Other;
            if let Some(flag) = parse_default_route(value) {
                default_route = Some(flag);
            }
        } else if let Some(value) = trimmed.strip_prefix("Protocols:") {
            // `Protocols: ±DefaultRoute ±LLMNR …`. Single-valued line. Used only as
            // the fallback default-route source (above) for output without the
            // explicit `Default Route:` line; otherwise it contributes nothing.
            section = Section::Other;
            protocols_default_route = parse_protocols_default_route(value);
        } else if looks_like_new_field(trimmed) {
            // Some other field (`Protocols:`, the `Link N (…)` header, …): it ends
            // any open continuation section but contributes nothing.
            section = Section::Other;
        } else {
            // A continuation line of the current multi-valued field.
            match section {
                Section::Servers => push_servers(trimmed, &mut servers),
                Section::Domains => push_domains(trimmed, &mut routing_domains),
                Section::Other => {}
            }
        }
    }

    LinkDnsState {
        servers,
        routing_domains,
        // The explicit `Default Route:` line wins; fall back to the `Protocols:`
        // `+DefaultRoute` token for older output that lacks the explicit line.
        default_route: default_route.or(protocols_default_route),
    }
}

/// Parse a `Default Route:` value (`yes` / `no`, case-insensitive) into a boolean;
/// any other token (or an empty value) yields `None` so an unrecognized form
/// leaves the flag unknown rather than guessing.
fn parse_default_route(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// Scan a `Protocols:` line's `±Flag` tokens for the default-route flag — a
/// fallback for older systemd-resolved output that omits the explicit
/// `Default Route:` line. `+DefaultRoute` → `Some(true)`, `-DefaultRoute` →
/// `Some(false)`, absent → `None`.
fn parse_protocols_default_route(values: &str) -> Option<bool> {
    values.split_whitespace().find_map(|token| match token {
        "+DefaultRoute" => Some(true),
        "-DefaultRoute" => Some(false),
        _ => None,
    })
}

/// Push the bare IP of each whitespace token of `values` onto `servers`,
/// de-duplicated. [`server_address`] is the defensive gate (like `query.rs`): a
/// non-address token — a label fragment, a header — yields `None` and is dropped
/// rather than recorded as a bogus server, and systemd's `:port` / `%ifname` /
/// `#SNI` decorations are stripped to the bare IP so the live read-back compares
/// equal to the daemon's believed plain IP (the *same* normalization the drift
/// comparison applies — see [`server_address`]).
fn push_servers(values: &str, servers: &mut Vec<String>) {
    for token in values.split_whitespace() {
        if let Some(ip) = server_address(token) {
            let ip = ip.to_string();
            if !servers.iter().any(|s| s == &ip) {
                servers.push(ip);
            }
        }
    }
}

/// Push each whitespace token of `values` onto `routing_domains`, stripping a
/// single leading `~` (the routing-only marker) so the result is the plain
/// domain, de-duplicated. A token that is only `~` (or empty) is skipped.
fn push_domains(values: &str, routing_domains: &mut Vec<String>) {
    for token in values.split_whitespace() {
        let domain = token.strip_prefix('~').unwrap_or(token);
        if !domain.is_empty() && !routing_domains.iter().any(|d| d == domain) {
            routing_domains.push(domain.to_string());
        }
    }
}

/// Whether a trimmed line begins a **new** `resolvectl status` field rather than
/// continuing a wrapped multi-valued one. A field line is `<Label>: …` (or the
/// `Link N (…)` block header); a continuation line carries only bare values.
///
/// We detect a label terminator — a `": "` (colon-space) or a sole trailing
/// `":"` — whose preceding text is label-shaped (ASCII letters/digits, spaces and
/// a few separators, no `.`). A domain token has no colon, and an IP token has no
/// `": "` and never ends in a label-shaped `:`, so neither a server nor a domain
/// continuation line is misread as a label. This matters mainly to terminate the
/// permissive `DNS Domain` section: server continuation lines are IP-gated anyway.
fn looks_like_new_field(trimmed: &str) -> bool {
    if trimmed.starts_with("Link ") {
        return true;
    }
    let label = match trimmed.split_once(": ") {
        Some((label, _)) => label,
        None => match trimmed.strip_suffix(':') {
            Some(label) => label,
            None => return false,
        },
    };
    !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '/' | '(' | ')' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic, placeholder-only fixtures — built by hand from the documented
    // `resolvectl status` *format*, never from a real machine's output (RFC 5737 /
    // RFC 3849 addresses, example domains, a generic interface name).

    #[test]
    fn parses_servers_and_bare_domain() {
        let output = "\
Link 5 (tun0)
    Current Scopes: DNS
         Protocols: +DefaultRoute +LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
Current DNS Server: 10.0.0.1
       DNS Servers: 10.0.0.1 10.0.0.2
        DNS Domain: example.com
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.servers, vec!["10.0.0.1", "10.0.0.2"]);
        assert_eq!(state.routing_domains, vec!["example.com"]);
        // No explicit `Default Route:` line here, so the flag falls back to the
        // `Protocols: +DefaultRoute` token → Some(true).
        assert_eq!(state.default_route, Some(true));
    }

    #[test]
    fn strips_tilde_routing_marker_from_domains() {
        // A routing-only domain is printed `~example.com`; the parser yields the
        // plain domain, and a mix of bare and `~`-prefixed is handled.
        let output = "\
Link 5 (tun0)
        DNS Domain: example.com ~corp.example.com
";
        let state = parse_resolvectl_status(output);
        assert_eq!(
            state.routing_domains,
            vec!["example.com", "corp.example.com"]
        );
    }

    #[test]
    fn collects_wrapped_servers_and_domains_across_continuation_lines() {
        // Both multi-valued fields wrap onto an aligned, label-less continuation
        // line; the values on those lines are still gathered.
        let output = "\
Link 5 (tun0)
       DNS Servers: 198.51.100.1 198.51.100.2
                    198.51.100.3
        DNS Domain: example.com ~corp.example.com
                    ~sub.example.net
     Default Route: yes
";
        let state = parse_resolvectl_status(output);
        assert_eq!(
            state.servers,
            vec!["198.51.100.1", "198.51.100.2", "198.51.100.3"]
        );
        assert_eq!(
            state.routing_domains,
            vec!["example.com", "corp.example.com", "sub.example.net"]
        );
        // The trailing `Default Route: yes` is captured (and does not bleed into
        // the wrapped domain list above it).
        assert_eq!(state.default_route, Some(true));
    }

    #[test]
    fn a_following_field_label_ends_the_domain_section() {
        // The permissive domain collector must stop at the next field label, not
        // swallow `Default Route: yes` as three bogus "domains" — while still
        // capturing the flag itself.
        let output = "\
        DNS Domain: example.com
     Default Route: yes
         LLMNR setting: yes
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.routing_domains, vec!["example.com"]);
        assert_eq!(state.default_route, Some(true));
    }

    #[test]
    fn rejects_non_ip_server_tokens() {
        // The `Fallback DNS Servers` header and any non-IP token must never be
        // recorded as a server; only IP-parseable tokens survive.
        let output = "\
       DNS Servers: 10.0.0.1 not-an-ip 2001:db8::1
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.servers, vec!["10.0.0.1", "2001:db8::1"]);
    }

    #[test]
    fn strips_server_token_decorations() {
        // systemd prints servers as ADDRESS[:PORT][%ifname]#SNI, IPv6-with-port
        // bracketed. The bare IP must be recovered from each form so a decorated
        // resolver is not dropped (which would mis-report as drift).
        let output = "\
Current DNS Server: 10.0.0.1#dns.example.com
       DNS Servers: 10.0.0.1#dns.example.com 198.51.100.2:53
                    [2001:db8::1]:53 2001:db8::2%tun0
";
        let state = parse_resolvectl_status(output);
        assert_eq!(
            state.servers,
            vec!["10.0.0.1", "198.51.100.2", "2001:db8::1", "2001:db8::2"]
        );
    }

    #[test]
    fn route_all_domain_parses_to_root() {
        // The default-route catch-all is printed `~.`; the `~` is stripped, so it
        // is recorded as the DNS root `.` (interpreted as route-all by the drift
        // comparison — see `compare_drift`).
        let output = "\
Link 5 (tun0)
        DNS Domain: ~.
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.routing_domains, vec!["."]);
    }

    #[test]
    fn ipv6_servers_including_double_colon_are_parsed() {
        // An IPv6 server (with `::`) on both the main and a continuation line must
        // parse and must not be mistaken for a label terminator.
        let output = "\
       DNS Servers: 2001:db8::1
                    2001:db8::2
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.servers, vec!["2001:db8::1", "2001:db8::2"]);
    }

    #[test]
    fn dedups_repeated_servers_and_domains() {
        // `Current DNS Server` usually repeats one of `DNS Servers`; the union is
        // de-duplicated.
        let output = "\
Current DNS Server: 10.0.0.1
       DNS Servers: 10.0.0.1 10.0.0.2
        DNS Domain: example.com example.com
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.servers, vec!["10.0.0.1", "10.0.0.2"]);
        assert_eq!(state.routing_domains, vec!["example.com"]);
    }

    #[test]
    fn empty_or_dnsless_output_parses_to_empty() {
        // A link with no per-link DNS (e.g. loopback) yields nothing.
        let output = "\
Link 1 (lo)
    Current Scopes: none
         Protocols: -DefaultRoute +LLMNR +mDNS -DNSOverTLS DNSSEC=no/unsupported
     Default Route: no
";
        let state = parse_resolvectl_status(output);
        assert!(state.servers.is_empty());
        assert!(state.routing_domains.is_empty());
        // `Default Route: no` is still captured even on a DNS-less link.
        assert_eq!(state.default_route, Some(false));

        let empty = parse_resolvectl_status("");
        assert!(empty.servers.is_empty());
        assert!(empty.routing_domains.is_empty());
        // No `Default Route:` line at all → unknown.
        assert_eq!(empty.default_route, None);
    }

    #[test]
    fn parses_default_route_flag_yes_no_and_unknown() {
        // The catch-all flag the split-DNS fix reads back: a full-tunnel link is
        // the DNS default route (`yes`) and resolves every unmatched name, which
        // `compare_drift` treats as a leak. `no` is the correct post-apply state.
        let yes = "\
Link 5 (tun0)
        DNS Domain: jira.example.com
     Default Route: yes
";
        assert_eq!(parse_resolvectl_status(yes).default_route, Some(true));

        let no = "\
Link 5 (tun0)
        DNS Domain: jira.example.com
     Default Route: no
";
        assert_eq!(parse_resolvectl_status(no).default_route, Some(false));

        // A case-variant value still parses (defensive against formatting drift).
        let mixed_case = "     Default Route: YES\n";
        assert_eq!(
            parse_resolvectl_status(mixed_case).default_route,
            Some(true)
        );

        // An unrecognized value leaves the flag unknown rather than guessing.
        let garbage = "     Default Route: maybe\n";
        assert_eq!(parse_resolvectl_status(garbage).default_route, None);
    }

    #[test]
    fn default_route_falls_back_to_protocols_token_then_explicit_line_wins() {
        // Older systemd-resolved output exposes the catch-all only via the
        // `Protocols:` token, with no explicit `Default Route:` line — the flag
        // must still be learned, or `compare_drift` would miss the leak.
        let protocols_only_on = "\
Link 5 (tun0)
         Protocols: +DefaultRoute +LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
        DNS Domain: jira.example.com
";
        assert_eq!(
            parse_resolvectl_status(protocols_only_on).default_route,
            Some(true)
        );

        let protocols_only_off = "\
Link 5 (tun0)
         Protocols: -DefaultRoute +LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
";
        assert_eq!(
            parse_resolvectl_status(protocols_only_off).default_route,
            Some(false)
        );

        // When both are present the explicit `Default Route:` line is authoritative
        // and overrides a disagreeing `Protocols:` token.
        let both_disagree = "\
Link 5 (tun0)
         Protocols: +DefaultRoute +LLMNR -mDNS -DNSOverTLS DNSSEC=no/unsupported
     Default Route: no
";
        assert_eq!(
            parse_resolvectl_status(both_disagree).default_route,
            Some(false)
        );
    }

    #[test]
    fn blank_line_ends_a_continuation_section() {
        // A blank line between the domain list and an unindented trailing token
        // must end the section so the token is not gathered as a domain.
        let output = "\
        DNS Domain: example.com

stray-trailing-token
";
        let state = parse_resolvectl_status(output);
        assert_eq!(state.routing_domains, vec!["example.com"]);
    }
}
