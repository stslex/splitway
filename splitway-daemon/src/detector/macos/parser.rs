//! Pure parsing of `scutil --dns` output — no I/O, unit tested.

/// Collect the DNS servers `scutil --dns` reports for `interface` (e.g.
/// `utun3`), in order of first appearance and de-duplicated. Returns an empty
/// vec when the interface has no resolver entry (the VPN is down).
///
/// `scutil --dns` groups settings into `resolver #N` blocks; a block's owning
/// interface is named in its `if_index : N (iface)` line, and its servers in
/// `nameserver[i] : addr` lines. The interface's resolver typically appears in
/// both the main and the "scoped queries" sections, so duplicates are expected
/// and dropped.
pub(super) fn parse_scutil_dns(output: &str, interface: &str) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    let mut block: Vec<String> = Vec::new();
    let mut block_iface: Option<String> = None;

    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("resolver #") {
            commit(&mut servers, &mut block, &block_iface, interface);
            block_iface = None;
        } else if let Some(rest) = line.strip_prefix("nameserver[") {
            // `nameserver[0] : 10.0.0.1` — require the `[` so only real
            // `nameserver[N]` keys match, never some other `nameserver*` key.
            if let Some((_, value)) = rest.split_once(':') {
                let ip = value.trim();
                if !ip.is_empty() {
                    block.push(ip.to_string());
                }
            }
        } else if let Some(rest) = line.strip_prefix("if_index") {
            // `if_index : 14 (utun3)` — take the name inside the parentheses.
            if let Some(name) = rest.split('(').nth(1).and_then(|s| s.split(')').next()) {
                block_iface = Some(name.trim().to_string());
            }
        }
    }
    commit(&mut servers, &mut block, &block_iface, interface);
    servers
}

/// Fold the just-parsed block into `servers` if it belongs to `interface`,
/// de-duplicating, then clear the block for the next one.
fn commit(
    servers: &mut Vec<String>,
    block: &mut Vec<String>,
    block_iface: &Option<String>,
    interface: &str,
) {
    if block_iface.as_deref() == Some(interface) {
        for ns in block.iter() {
            if !servers.contains(ns) {
                servers.push(ns.clone());
            }
        }
    }
    block.clear();
}

#[cfg(test)]
mod tests {
    use super::parse_scutil_dns;

    const SCUTIL: &str = "\
DNS configuration

resolver #1
  search domain[0] : lan
  nameserver[0] : 192.168.1.1
  if_index : 4 (en0)
  flags    : Request A records, Request AAAA records
  reach    : 0x00000002 (Reachable)

resolver #2
  domain   : corp.example.com
  nameserver[0] : 10.0.0.1
  nameserver[1] : 10.0.0.2
  if_index : 14 (utun3)
  flags    : Request A records
  order    : 102400

DNS configuration (for scoped queries)

resolver #1
  nameserver[0] : 192.168.1.1
  if_index : 4 (en0)
  flags    : Scoped, Request A records

resolver #2
  nameserver[0] : 10.0.0.1
  nameserver[1] : 10.0.0.2
  if_index : 14 (utun3)
  flags    : Scoped, Request A records
";

    #[test]
    fn collects_vpn_interface_servers_deduped() {
        // utun3 appears in both the main and scoped sections; dedup to one set,
        // order preserved.
        assert_eq!(
            parse_scutil_dns(SCUTIL, "utun3"),
            vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()]
        );
    }

    #[test]
    fn collects_other_interface_independently() {
        assert_eq!(
            parse_scutil_dns(SCUTIL, "en0"),
            vec!["192.168.1.1".to_string()]
        );
    }

    #[test]
    fn absent_interface_yields_empty() {
        assert!(parse_scutil_dns(SCUTIL, "utun9").is_empty());
        assert!(parse_scutil_dns("", "utun3").is_empty());
    }

    #[test]
    fn nameservers_without_matching_if_index_are_ignored() {
        // A block with servers but no if_index line is attributed to no
        // interface, so nothing is collected.
        let output = "resolver #1\n  nameserver[0] : 1.2.3.4\n";
        assert!(parse_scutil_dns(output, "utun3").is_empty());
    }

    #[test]
    fn attributes_when_if_index_is_the_last_line_of_the_block() {
        // The buffering invariant: nameservers parsed before the if_index line
        // are still attributed to the interface named by that later line.
        let output = "\
resolver #1
  nameserver[0] : 10.0.0.1
  nameserver[1] : 10.0.0.2
  if_index : 14 (utun3)
";
        assert_eq!(
            parse_scutil_dns(output, "utun3"),
            vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()]
        );
    }

    #[test]
    fn preserves_ipv6_and_double_digit_indices() {
        // split_once(':') keeps the full IPv6 address (and %zone) intact, and
        // double-digit nameserver indices parse like single-digit ones.
        let output = "\
resolver #1
  nameserver[0] : 2001:db8::1
  nameserver[1] : fe80::1%utun3
  nameserver[10] : 10.0.0.1
  if_index : 14 (utun3)
";
        assert_eq!(
            parse_scutil_dns(output, "utun3"),
            vec![
                "2001:db8::1".to_string(),
                "fe80::1%utun3".to_string(),
                "10.0.0.1".to_string(),
            ]
        );
    }

    #[test]
    fn interface_match_is_exact_not_substring() {
        // utun3 must not match utun30.
        let output = "\
resolver #1
  nameserver[0] : 10.0.0.1
  if_index : 14 (utun30)
";
        assert!(parse_scutil_dns(output, "utun3").is_empty());
        assert_eq!(
            parse_scutil_dns(output, "utun30"),
            vec!["10.0.0.1".to_string()]
        );
    }
}
