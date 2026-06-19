use splitway_shared::platform::PlatformError;

/// Parse DNS server addresses from `nmcli device show <interface>` output.
///
/// Collects every `IP4.DNS[n]` and `IP6.DNS[n]` entry in order of appearance.
pub(crate) fn parse_dns_from_nmcli(output: &str) -> Result<Vec<String>, PlatformError> {
    let servers: Vec<String> = output
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
        .collect();

    if servers.is_empty() {
        return Err(PlatformError::ParseError(
            "no IP4.DNS/IP6.DNS entries found in nmcli output".to_string(),
        ));
    }

    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::parse_dns_from_nmcli;
    use splitway_shared::platform::PlatformError;

    #[test]
    fn single_dns_entry() {
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
        let dns = parse_dns_from_nmcli(output).unwrap();
        assert_eq!(dns, vec!["192.0.2.1".to_string()]);
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
        let dns = parse_dns_from_nmcli(output).unwrap();
        assert_eq!(dns, vec!["10.8.0.1".to_string(), "10.8.0.2".to_string()]);
    }

    #[test]
    fn ipv6_dns_entries() {
        let output = "\
GENERAL.DEVICE:                         wg0
GENERAL.TYPE:                           wireguard
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.DNS[1]:                             192.0.2.1
IP6.ADDRESS[1]:                         fd00::2/128
IP6.DNS[1]:                             fd00::1
IP6.GATEWAY:                            --
";
        let dns = parse_dns_from_nmcli(output).unwrap();
        assert_eq!(dns, vec!["192.0.2.1".to_string(), "fd00::1".to_string()]);
    }

    /// Synthetic fixture modeled on `nmcli device show tun0` output for a
    /// NetworkManager-managed VPN tunnel (e.g. openconnect/GlobalProtect). NM
    /// models every VPN plugin's `tun*` device the same way and normalizes
    /// pushed DNS into the device's `IP4.DNS[n]` fields, so this is exactly the
    /// field layout an OpenVPN-over-NM `tun*` device exposes. The device reports
    /// `connected (externally)` (NMDeviceState 100) because the VPN client, not
    /// NM, created it — the parser must still pick up the pushed DNS. Routes are
    /// trimmed for brevity; the parser ignores them regardless. All addresses
    /// here are RFC 5737 / RFC 3849 documentation placeholders.
    #[test]
    fn openvpn_over_nm_tun_device() {
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
IP4.ROUTE[1]:                           dst = 192.0.2.1/32, nh = 0.0.0.0, mt = 50
IP4.ROUTE[2]:                           dst = 198.51.100.0/24, nh = 0.0.0.0, mt = 50
IP4.DNS[1]:                             192.0.2.1
IP6.ADDRESS[1]:                         fe80::1/64
IP6.GATEWAY:                            --
IP6.ROUTE[1]:                           dst = fe80::/64, nh = ::, mt = 256
";
        let dns = parse_dns_from_nmcli(output).unwrap();
        assert_eq!(dns, vec!["192.0.2.1".to_string()]);
    }

    #[test]
    fn no_dns_entries_is_parse_error() {
        let output = "\
GENERAL.DEVICE:                         eth0
GENERAL.TYPE:                           ethernet
GENERAL.STATE:                          100 (connected)
IP4.ADDRESS[1]:                         192.168.1.10/24
IP4.GATEWAY:                            192.168.1.1
";
        let err = parse_dns_from_nmcli(output).unwrap_err();
        assert!(matches!(err, PlatformError::ParseError(_)));
    }

    #[test]
    fn empty_input_is_parse_error() {
        let err = parse_dns_from_nmcli("").unwrap_err();
        assert!(matches!(err, PlatformError::ParseError(_)));
    }
}
