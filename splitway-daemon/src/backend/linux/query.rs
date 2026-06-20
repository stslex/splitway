//! Pure parser for `resolvectl query <host>` output, kept I/O-free so it is
//! unit-tested with synthetic fixtures. The backend shells out and hands the
//! captured stdout here.
//!
//! Observed format (systemd-resolved): one line per resolved address, each
//! ending in `-- link: <iface>`; the first line is prefixed with `<host>:`; a
//! trailing block of `-- ...` metadata lines (protocol, authentication) follows.
//! The resolver IP is not reported, so `via_dns` is always `None` here.
//!
//! Defensive: a candidate address is the last whitespace token of a line (before
//! any `-- link:`), kept only if it parses as an IP address — so the `<host>:`
//! prefix and the `-- Information …` metadata lines never contribute a bogus
//! "address". `via_interface` comes from `-- link:` when present and degrades to
//! `None` when it is absent (the address is still captured).

use std::net::IpAddr;

use splitway_shared::ipc::ResolutionInfo;

/// Parse the addresses and (when reported) the answering link from
/// `resolvectl query` stdout. Never fails — an empty result means nothing
/// parsed, which the caller treats as "no resolution".
pub(crate) fn parse_resolvectl_query(output: &str) -> ResolutionInfo {
    let mut addresses: Vec<String> = Vec::new();
    let mut via_interface: Option<String> = None;

    for line in output.lines() {
        // Split off the `-- link: <iface>` annotation when present; the address
        // is in the part before it (or the whole line when absent).
        let (addr_part, link) = match line.split_once("-- link:") {
            Some((before, after)) => (before, Some(after.trim())),
            None => (line, None),
        };

        // The address is the last whitespace token — the first line is
        // `<host>: <addr>` and continuation lines are just `<addr>`, so the host
        // prefix is an earlier token. Validate it as an IP so the host token and
        // the trailing `-- Information …` metadata lines never slip through; this
        // also preserves a `::`-terminated IPv6 address verbatim.
        let Some(token) = addr_part.split_whitespace().next_back() else {
            continue;
        };
        if token.parse::<IpAddr>().is_err() {
            continue;
        }
        if !addresses.iter().any(|a| a == token) {
            addresses.push(token.to_string());
        }
        // Attribution is keyed on the FIRST reported link, by design: under
        // systemd-resolved's per-name link routing all answers for one query
        // share a link. If a future resolver ever split answers across links
        // (e.g. A via one, AAAA via another) this samples only the first — and
        // the CLI's "not resolving through the VPN" verdict would then key off
        // that single sample.
        if via_interface.is_none() {
            if let Some(iface) = link {
                if !iface.is_empty() {
                    via_interface = Some(iface.to_string());
                }
            }
        }
    }

    ResolutionInfo {
        addresses,
        via_interface,
        // `resolvectl query` does not report the resolver IP.
        via_dns: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic, placeholder-only fixtures (RFC 5737 / RFC 3849 addresses,
    // example domains, generic interface names) — never a real machine's output.
    #[test]
    fn parses_addresses_and_link() {
        // Includes a `::`-terminated IPv6 address to guard against truncation.
        let output = "\
vault.sub.example.com: 2001:db8::                          -- link: tun0
                        198.51.100.10                       -- link: tun0
                        198.51.100.11                       -- link: tun0

-- Information acquired via protocol DNS in 12.3ms.
-- Data is authenticated: no
";
        let info = parse_resolvectl_query(output);
        assert_eq!(
            info.addresses,
            vec![
                "2001:db8::".to_string(),
                "198.51.100.10".to_string(),
                "198.51.100.11".to_string(),
            ]
        );
        assert_eq!(info.via_interface.as_deref(), Some("tun0"));
        assert_eq!(info.via_dns, None);
    }

    #[test]
    fn captures_address_but_degrades_link_when_absent() {
        // A format without the `-- link:` annotation still yields the address;
        // attribution simply degrades to None (the address is not dropped).
        let output = "example.com: 203.0.113.5\n";
        let info = parse_resolvectl_query(output);
        assert_eq!(info.addresses, vec!["203.0.113.5".to_string()]);
        assert_eq!(info.via_interface, None);
    }

    #[test]
    fn skips_metadata_and_host_tokens() {
        // The `<host>:` prefix token and the trailing `-- ...` metadata lines must
        // never be mistaken for an address (they do not parse as IPs).
        let output = "\
example.com: 203.0.113.5   -- link: tun0

-- Information acquired via protocol DNS in 4.0ms.
-- Data is authenticated: no
";
        let info = parse_resolvectl_query(output);
        assert_eq!(info.addresses, vec!["203.0.113.5".to_string()]);
    }

    #[test]
    fn empty_output_parses_to_empty() {
        let info = parse_resolvectl_query("");
        assert!(info.addresses.is_empty());
        assert_eq!(info.via_interface, None);
        assert_eq!(info.via_dns, None);
    }

    #[test]
    fn dedups_repeated_addresses() {
        let output = "\
example.com: 203.0.113.5   -- link: tun0
             203.0.113.5   -- link: tun0
";
        let info = parse_resolvectl_query(output);
        assert_eq!(info.addresses, vec!["203.0.113.5".to_string()]);
    }
}
