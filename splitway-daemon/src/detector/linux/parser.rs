//! Pure parsers over `nmcli` output. No process execution lives here, so every
//! function is unit-testable against captured text.

/// Extract DNS server addresses from `nmcli` key/value output — either `nmcli
/// device show <iface>` or `nmcli connection show <uuid>`.
///
/// Collects every `IP4.DNS[n]` and `IP6.DNS[n]` entry in order of appearance.
///
/// Absence of any such entry is a **valid state, not a parse error**: a VPN can
/// push routes but no DNS, and openconnect/GlobalProtect attaches its pushed DNS
/// to the VPN active connection rather than the tun device — so this returns an
/// empty `Vec` rather than erroring. Callers decide what an empty result means
/// (see [`super::detector`]'s fallback chain).
///
/// It keys on the bracketed `IP4.DNS[` / `IP6.DNS[` runtime form, so the static
/// `ipv4.dns:` / `ipv6.dns:` settings lines in `nmcli connection show` output
/// are ignored.
pub(crate) fn extract_dns_from_nmcli(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            if !key.starts_with("IP4.DNS[") && !key.starts_with("IP6.DNS[") {
                return None;
            }
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        })
        .collect()
}

/// Parse `nmcli -t -f UUID,TYPE,STATE connection show --active` terse output,
/// returning the UUIDs of every active (`activated`) VPN connection.
///
/// Terse (`-t`) output is colon-separated. None of UUID / TYPE / STATE can hold
/// a literal colon (a UUID is hex-and-dashes, the type and state are fixed
/// tokens), so a plain split is safe and no de-escaping is needed.
pub(crate) fn parse_active_vpn_uuids(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ':');
            let uuid = fields.next()?;
            let typ = fields.next()?;
            let state = fields.next()?;
            if typ == "vpn" && state == "activated" && !uuid.is_empty() {
                Some(uuid.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_dns_from_nmcli, parse_active_vpn_uuids};

    #[test]
    fn single_dns_entry() {
        // WireGuard-style: NM does put pushed DNS on the device for this plugin,
        // so the device-level extractor (step 1 of the fallback chain) finds it.
        let output = "\
GENERAL.DEVICE:                         wg0
GENERAL.TYPE:                           wireguard
GENERAL.HWADDR:                         (unknown)
GENERAL.MTU:                            1420
GENERAL.STATE:                          100 (connected)
GENERAL.CONNECTION:                     wg0
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.GATEWAY:                            --
IP4.ROUTE[1]:                           dst = 192.0.2.0/24, nh = 0.0.0.0, mt = 50
IP4.DNS[1]:                             192.0.2.1
IP6.GATEWAY:                            --
";
        assert_eq!(
            extract_dns_from_nmcli(output),
            vec!["192.0.2.1".to_string()]
        );
    }

    #[test]
    fn multiple_dns_entries() {
        let output = "\
GENERAL.DEVICE:                         tun0
GENERAL.TYPE:                           tun
GENERAL.STATE:                          100 (connected)
IP4.ADDRESS[1]:                         10.8.0.6/24
IP4.DNS[1]:                             10.8.0.1
IP4.DNS[2]:                             10.8.0.2
IP4.DOMAIN[1]:                          corp.example.com
IP6.GATEWAY:                            --
";
        assert_eq!(
            extract_dns_from_nmcli(output),
            vec!["10.8.0.1".to_string(), "10.8.0.2".to_string()]
        );
    }

    #[test]
    fn ipv6_dns_entries() {
        let output = "\
GENERAL.DEVICE:                         wg0
GENERAL.TYPE:                           wireguard
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.DNS[1]:                             192.0.2.1
IP6.ADDRESS[1]:                         2001:db8::2/128
IP6.DNS[1]:                             2001:db8::1
IP6.GATEWAY:                            --
";
        assert_eq!(
            extract_dns_from_nmcli(output),
            vec!["192.0.2.1".to_string(), "2001:db8::1".to_string()]
        );
    }

    /// Synthetic fixture modeled on `nmcli device show tun0` for a
    /// NetworkManager-managed openconnect/GlobalProtect tunnel. Unlike
    /// WireGuard, NM does **not** attach the pushed DNS to the tun device for
    /// this VPN family — only the pushed *routes* land here; the DNS lives on
    /// the VPN active connection (see [`vpn_connection_show_has_dns`]). The
    /// device-level extractor must therefore return an **empty** `Vec` with no
    /// error. The device reports `connected (externally)` (NMDeviceState 100)
    /// because the VPN client, not NM, created it. All addresses are RFC 5737 /
    /// RFC 3849 documentation placeholders.
    #[test]
    fn gp_device_has_routes_but_no_dns() {
        let output = "\
GENERAL.DEVICE:                         tun0
GENERAL.TYPE:                           tun
GENERAL.NM-TYPE:                        NMDeviceTun
GENERAL.MTU:                            1422
GENERAL.STATE:                          100 (connected (externally))
GENERAL.IP-IFACE:                       tun0
GENERAL.IS-SOFTWARE:                    yes
GENERAL.NM-MANAGED:                     yes
GENERAL.CONNECTION:                     tun0
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.GATEWAY:                            --
IP4.ROUTE[1]:                           dst = 198.51.100.0/24, nh = 0.0.0.0, mt = 50
IP4.ROUTE[2]:                           dst = 203.0.113.0/24, nh = 0.0.0.0, mt = 50
IP4.ROUTE[3]:                           dst = 10.8.0.0/24, nh = 0.0.0.0, mt = 50
IP6.ADDRESS[1]:                         2001:db8::2/64
IP6.GATEWAY:                            --
IP6.ROUTE[1]:                           dst = 2001:db8::/64, nh = ::, mt = 256
";
        assert!(extract_dns_from_nmcli(output).is_empty());
    }

    /// Synthetic fixture modeled on `nmcli connection show <vpn-uuid>` for the
    /// active openconnect/GlobalProtect connection. Its runtime `IP4.DNS[n]` /
    /// `IP6.DNS[n]` lines carry the pushed resolvers (step 2 of the fallback
    /// chain). The static `ipv4.dns:` settings line must be ignored — the
    /// extractor keys on the bracketed `IP4.DNS[` runtime form. RFC 5737 / 3849
    /// placeholders only.
    #[test]
    fn vpn_connection_show_has_dns() {
        let output = "\
connection.id:                          corp-vpn
connection.type:                        vpn
connection.interface-name:              --
vpn.service-type:                       org.freedesktop.NetworkManager.openconnect
ipv4.dns:                               --
ipv6.dns:                               --
VPN.TYPE:                               openconnect
VPN.VPN-STATE:                          5 (VPN connected)
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.DNS[1]:                             203.0.113.53
IP4.DNS[2]:                             203.0.113.54
IP6.DNS[1]:                             2001:db8::53
";
        assert_eq!(
            extract_dns_from_nmcli(output),
            vec![
                "203.0.113.53".to_string(),
                "203.0.113.54".to_string(),
                "2001:db8::53".to_string(),
            ]
        );
    }

    #[test]
    fn no_dns_entries_returns_empty() {
        let output = "\
GENERAL.DEVICE:                         eth0
GENERAL.TYPE:                           ethernet
GENERAL.STATE:                          100 (connected)
IP4.ADDRESS[1]:                         192.0.2.10/24
IP4.GATEWAY:                            192.0.2.1
";
        assert!(extract_dns_from_nmcli(output).is_empty());
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(extract_dns_from_nmcli("").is_empty());
    }

    #[test]
    fn active_vpn_uuids_picks_single_activated_vpn() {
        let output = "\
11111111-1111-1111-1111-111111111111:vpn:activated
22222222-2222-2222-2222-222222222222:ethernet:activated
33333333-3333-3333-3333-333333333333:wifi:activated
";
        assert_eq!(
            parse_active_vpn_uuids(output),
            vec!["11111111-1111-1111-1111-111111111111".to_string()]
        );
    }

    #[test]
    fn active_vpn_uuids_returns_all_active_vpns() {
        let output = "\
11111111-1111-1111-1111-111111111111:vpn:activated
22222222-2222-2222-2222-222222222222:ethernet:activated
33333333-3333-3333-3333-333333333333:vpn:activated
";
        assert_eq!(
            parse_active_vpn_uuids(output),
            vec![
                "11111111-1111-1111-1111-111111111111".to_string(),
                "33333333-3333-3333-3333-333333333333".to_string(),
            ]
        );
    }

    #[test]
    fn active_vpn_uuids_ignores_non_activated_vpn() {
        // A VPN still activating (not yet `activated`) must not count.
        let output = "\
11111111-1111-1111-1111-111111111111:vpn:activating
22222222-2222-2222-2222-222222222222:ethernet:activated
";
        assert!(parse_active_vpn_uuids(output).is_empty());
    }

    #[test]
    fn active_vpn_uuids_empty_when_no_vpn() {
        let output = "\
22222222-2222-2222-2222-222222222222:ethernet:activated
";
        assert!(parse_active_vpn_uuids(output).is_empty());
    }

    #[test]
    fn active_vpn_uuids_tolerates_blank_lines() {
        let output = "\
11111111-1111-1111-1111-111111111111:vpn:activated

";
        assert_eq!(
            parse_active_vpn_uuids(output),
            vec!["11111111-1111-1111-1111-111111111111".to_string()]
        );
    }
}
