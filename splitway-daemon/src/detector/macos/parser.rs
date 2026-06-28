//! Pure parsing of the macOS SystemConfiguration DNS model — no I/O, unit
//! tested. This is the structural, vendor-neutral core of macOS VPN detection.
//!
//! # Why not `scutil --dns`, and why per-service (not the global default)
//!
//! The original detector filtered `scutil --dns` by a chosen `utun*` interface.
//! That fails against a VPN client that hijacks the system **default** resolver
//! instead of scoping its DNS to the tunnel: there is then *no* resolver scoped
//! to any `utun` (the active tunnel `utun` index even varies between sessions),
//! so an interface-keyed read finds nothing. We instead read the SystemConfig
//! dynamic store directly and decide structurally over the **per-service** DNS:
//!
//! - `State:/Network/Global/IPv4` `PrimaryInterface` / `PrimaryService` — the
//!   physical primary interface (e.g. `en0`) and its service id.
//! - each `State:/Network/Service/<id>/DNS` — that service's `ServerAddresses`
//!   and `InterfaceName`.
//!
//! The **physical service** is the one whose id is the primary service (falling
//! back to the primary interface name); its resolver is the demote-target. A
//! **VPN service** is any *other* service that (a) carries DNS differing from the
//! physical resolver AND (b) is plausibly the **default-resolver hijacker** —
//! i.e. it rides the primary interface's own default route, runs on a tunnel
//! pseudo-interface (`utun`/`ppp`/`ipsec`/…), or reports no interface (an
//! unscoped/default resolver). A service bound to a *distinct hardware* interface
//! (a second Ethernet/Wi-Fi/cellular link while another is primary) is a parallel
//! physical network, not a hijacker: its differing DHCP resolver must **not** read
//! as "VPN up". When such a hijacker service exists → **VPN is up** and its
//! resolver is the corp DNS. The decision keys on this structural *difference* and
//! on interface *kind* (the stable BSD driver prefix), never on any vendor/product
//! string, so it generalises across VPN clients.
//!
//! Detection deliberately does **not** read `State:/Network/Global/DNS`: Splitway's
//! own demote overwrites the physical service's DNS, which can change the global
//! default — so a Global-keyed detector would flip to "down" the instant our
//! demote took effect, then revert → the VPN re-asserts → re-demote → oscillation.
//! Reading the VPN's corp DNS from its *own* service entry (which our demote does
//! not touch) keeps detection stable while the demote is in effect.

/// One network service's DNS entry, as read from `State:/Network/Service/<id>/DNS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServiceDns {
    /// The service id (the `<id>` in the `State:/Network/Service/<id>/DNS` key).
    /// The authoritative anchor for the *physical* service: it equals the
    /// `PrimaryService` from `State:/Network/Global/IPv4`. Preferred over the
    /// interface name, which a VPN service can also report.
    pub service_id: String,
    /// The `InterfaceName` the service is bound to (e.g. `en0`), if present.
    /// The fallback anchor for the physical service when the primary service id
    /// is unknown; a VPN service is otherwise identified structurally, not by
    /// name.
    pub interface_name: Option<String>,
    /// The service's `ServerAddresses`.
    pub servers: Vec<String>,
}

/// The structural DNS picture read from the dynamic store, before the up/down
/// decision. All fields are plain parsed data — the decision in [`decide`] is a
/// separate pure step so both halves are independently testable.
///
/// Detection is driven by the **per-service** entries, NOT by
/// `State:/Network/Global/DNS`. That is deliberate: Splitway's own *demote*
/// overwrites the primary (physical) service's DNS, which can change the global
/// default — so a detector keyed on Global would flip to "down" the moment our
/// demote took effect (Global == physical), then revert → the VPN re-asserts →
/// re-demote → oscillation. Reading the VPN's corp DNS from its *own* service
/// entry (which our demote does not touch) keeps detection stable while the
/// demote is in effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DnsModel {
    /// `State:/Network/Global/IPv4` `PrimaryInterface` (e.g. `en0`). `None` if
    /// the key or field is absent (no primary network — effectively offline).
    pub primary_interface: Option<String>,
    /// `State:/Network/Global/IPv4` `PrimaryService` (the primary service id).
    /// The authoritative anchor for the physical service in [`decide`]; a VPN
    /// service can also report the primary interface name, so the service id is
    /// preferred and the interface name is only a fallback.
    pub primary_service: Option<String>,
    /// Every network service's DNS entry (from the per-service DNS keys). The
    /// physical service is the one whose `service_id` is the primary service
    /// (or, failing that, whose `interface_name` is the primary interface); a
    /// VPN service is one whose DNS differs from the physical service's.
    pub services: Vec<ServiceDns>,
}

/// The detector's verdict over a [`DnsModel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Detected {
    /// A non-physical (VPN) service is in play → VPN up. `corp_dns` is that
    /// service's resolver (queries for the corp domains go here, on-tunnel);
    /// `demote_target` is where non-corp DNS must be sent so it resolves
    /// off-tunnel (the physical interface's own DHCP resolver).
    Up {
        corp_dns: Vec<String>,
        demote_target: Vec<String>,
    },
    /// No non-physical service (or no primary network) → no VPN. Nothing to do.
    Down,
}

/// Parse one `scutil` dictionary dump (the text emitted by `show <key>` /
/// `get <key>` + `d.show`) and return the values of a named array key.
///
/// The dump form (placeholder values):
/// ```text
/// <dictionary> {
///   ServerAddresses : <array> {
///     0 : 192.0.2.53
///     1 : 192.0.2.54
///   }
///   __CONFIGURATION_ID__ : Default: ...
///   PrimaryInterface : en0
/// }
/// ```
/// Returns the array elements in order (here for `ServerAddresses`:
/// `["192.0.2.53", "192.0.2.54"]`). An absent key yields an empty vec.
pub(crate) fn parse_array_field(dump: &str, field: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut in_array = false;
    // The `<array>` opener line for the field we want, e.g.
    // `ServerAddresses : <array> {`.
    let opener = format!("{field} :");
    for raw in dump.lines() {
        let line = raw.trim();
        if !in_array {
            // Enter the array only for an exact `<field> : <array> {` opener, so
            // a same-prefixed field (e.g. `ServerAddressesV6`) is not matched.
            if line.starts_with(&opener) && line.contains("<array>") && line.ends_with('{') {
                in_array = true;
            }
            continue;
        }
        if line == "}" {
            break; // end of the array
        }
        // Array element line: `<index> : <value>`. Keep the value verbatim
        // (IPv6 with `::` and `%zone` stay intact via split_once on the FIRST
        // colon only).
        if let Some((idx, value)) = line.split_once(':') {
            if idx.trim().chars().all(|c| c.is_ascii_digit()) && !idx.trim().is_empty() {
                let v = value.trim();
                if !v.is_empty() {
                    values.push(v.to_string());
                }
            }
        }
    }
    values
}

/// Parse a scalar string field from a `scutil` dictionary dump, e.g.
/// `PrimaryInterface : en0` → `Some("en0")`. Returns `None` if absent.
pub(crate) fn parse_scalar_field(dump: &str, field: &str) -> Option<String> {
    let opener = format!("{field} :");
    for raw in dump.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix(&opener) {
            // A scalar line is `Field : value`; reject the array opener form so
            // `Foo : <array> {` is never read as the scalar string "<array> {".
            let v = rest.trim();
            if v.contains("<array>") || v.contains("<dictionary>") {
                return None;
            }
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The structural decision: is a non-physical (VPN) service in play? Pure over a
/// fully-assembled [`DnsModel`], reading the **per-service** entries (not the
/// mutable global default — see [`DnsModel`]).
///
/// The physical service is the one bound to the primary interface; its DNS is
/// the demote-target. A **VPN service** is any *other* service that both carries
/// DNS differing from the physical service's AND looks like the default-resolver
/// hijacker (see [`is_default_resolver_hijacker`]) — so a benign secondary
/// physical network is not mistaken for a VPN. VPN is **up** iff such a service
/// exists, and `corp_dns` is its resolver.
///
/// Edge cases, all → [`Detected::Down`] (no false-positive apply):
/// - no primary interface (offline) — nothing to anchor on;
/// - no physical service DNS found — cannot determine the demote-target, and a
///   demote to nothing is worse than not applying, so stay conservative;
/// - the only services differing from the physical are distinct secondary
///   physical networks (not the hijacker) — no VPN.
pub(super) fn decide(model: &DnsModel) -> Detected {
    // No primary network at all → nothing is in play.
    // A primary network must exist to anchor on (offline → nothing in play).
    if model.primary_interface.is_none() && model.primary_service.is_none() {
        return Detected::Down;
    }

    // The physical service, anchored authoritatively by the primary SERVICE id
    // when known (a VPN service can also report the primary interface name, so
    // the id is preferred), falling back to the primary interface name. Its
    // resolver is the demote-target (where non-corp DNS goes off-tunnel).
    let physical = model.services.iter().find(|s| {
        !s.servers.is_empty()
            && match model.primary_service.as_deref() {
                Some(id) => s.service_id == id,
                None => s.interface_name.as_deref() == model.primary_interface.as_deref(),
            }
    });
    let Some(physical) = physical else {
        // Without the physical resolver we cannot pick a safe demote-target.
        return Detected::Down;
    };

    // A VPN service: a service OTHER than the physical one, carrying DNS that
    // differs from the physical resolver (a non-physical resolver is in play).
    // Excluding the physical service by id means the comparison survives our own
    // demote (which sets the physical service's DNS to the fallback) — the VPN's
    // own service still differs — and never mistakes the physical service for a
    // VPN. This signal is independent of the mutable global default.
    //
    // The differing service must ALSO be the default-resolver hijacker, not a
    // distinct secondary physical network (e.g. Wi-Fi associated while Ethernet is
    // primary, with its own DHCP DNS) — otherwise that benign secondary resolver
    // would be a false "VPN up". The hijacker filter precedes the difference check
    // so, with both a real VPN and a secondary network present, the VPN is still
    // found regardless of service order.
    let vpn_dns = model
        .services
        .iter()
        .filter(|s| s.service_id != physical.service_id && !s.servers.is_empty())
        .filter(|s| is_default_resolver_hijacker(s, model.primary_interface.as_deref()))
        .find(|s| !same_set(&s.servers, &physical.servers))
        .map(|s| &s.servers);

    match vpn_dns {
        Some(corp_dns) => Detected::Up {
            corp_dns: corp_dns.clone(),
            demote_target: physical.servers.clone(),
        },
        None => Detected::Down,
    }
}

/// Order-insensitive equality of two resolver lists (treated as sets).
fn same_set(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_sorted: Vec<&String> = a.iter().collect();
    let mut b_sorted: Vec<&String> = b.iter().collect();
    a_sorted.sort();
    b_sorted.sort();
    a_sorted == b_sorted
}

/// macOS tunnel / virtual interface name prefixes. A global-default-hijack VPN
/// rides one of these pseudo-interfaces (or the primary interface itself); a
/// *secondary physical* network (Wi-Fi while Ethernet is primary, a second
/// Ethernet, cellular, a VM/Thunderbolt bridge, …) is bound to a distinct
/// hardware interface that is none of these. Matching by interface *kind* (the
/// stable BSD driver prefix) — never a vendor/product string — keeps the detector
/// vendor-neutral while still excluding parallel physical networks.
const TUNNEL_INTERFACE_PREFIXES: [&str; 5] = ["utun", "ppp", "ipsec", "tun", "tap"];

/// Whether an interface name is a tunnel / virtual pseudo-interface (a VPN
/// carrier) rather than a hardware link.
fn is_tunnel_interface(iface: &str) -> bool {
    TUNNEL_INTERFACE_PREFIXES
        .iter()
        .any(|prefix| iface.starts_with(prefix))
}

/// Whether a (non-physical) service is plausibly the **default-resolver
/// hijacker** rather than a benign secondary physical network.
///
/// A VPN that hijacks the system default resolver either rides the primary
/// interface's own default route (its service reports the *primary* interface),
/// runs on a tunnel pseudo-interface (`utun`/`ppp`/`ipsec`/…), or registers an
/// unscoped/default resolver (its service reports *no* interface). A service
/// bound to a *distinct hardware* interface is a parallel physical network — a
/// second Ethernet/Wi-Fi/cellular link — and its differing DHCP resolver must NOT
/// read as "VPN up". This signal is stable under Splitway's own demote (it does
/// not depend on the mutable global default).
fn is_default_resolver_hijacker(service: &ServiceDns, primary_interface: Option<&str>) -> bool {
    match service.interface_name.as_deref() {
        // No interface → an unscoped / default (global-path) resolver.
        None => true,
        // Rides the primary default route, or is a VPN tunnel pseudo-interface.
        // Anything else is a distinct secondary physical link → not a hijacker.
        Some(iface) => Some(iface) == primary_interface || is_tunnel_interface(iface),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The breaking shape (synthetic placeholders): the corp DNS is the system
    // DEFAULT (no domain / no if_index), and the only scoped resolver is the
    // physical interface — there is NO utun-scoped resolver. Mirrors a real
    // global-DNS-hijack VPN client. Values are RFC 5737 placeholders.
    const GLOBAL_DNS: &str = "\
<dictionary> {
  ServerAddresses : <array> {
    0 : 192.0.2.53
  }
  __CONFIGURATION_ID__ : Default: vpn-service 0
  SearchOrder : 50000
}";

    const GLOBAL_IPV4: &str = "\
<dictionary> {
  PrimaryInterface : en0
  PrimaryService : 0AFE20D4-0000-0000-0000-PLACEHOLDER01
  Router : 198.51.100.1
}";

    // The physical interface's own DHCP resolver — the demote-target source.
    const PHYSICAL_DNS: &str = "\
<dictionary> {
  ServerAddresses : <array> {
    0 : 198.51.100.1
  }
}";

    #[test]
    fn parses_server_addresses_array() {
        assert_eq!(
            parse_array_field(GLOBAL_DNS, "ServerAddresses"),
            vec!["192.0.2.53".to_string()]
        );
    }

    #[test]
    fn parses_multiple_server_addresses_in_order() {
        let dump = "\
<dictionary> {
  ServerAddresses : <array> {
    0 : 192.0.2.53
    1 : 192.0.2.54
  }
}";
        assert_eq!(
            parse_array_field(dump, "ServerAddresses"),
            vec!["192.0.2.53".to_string(), "192.0.2.54".to_string()]
        );
    }

    #[test]
    fn array_field_absent_yields_empty() {
        assert!(parse_array_field(GLOBAL_IPV4, "ServerAddresses").is_empty());
        assert!(parse_array_field("", "ServerAddresses").is_empty());
    }

    #[test]
    fn same_prefixed_array_field_is_not_matched() {
        // A `ServerAddressesV6` array must not be read as `ServerAddresses`.
        let dump = "\
<dictionary> {
  ServerAddressesV6 : <array> {
    0 : 2001:db8::1
  }
}";
        assert!(parse_array_field(dump, "ServerAddresses").is_empty());
    }

    #[test]
    fn preserves_ipv6_values_intact() {
        let dump = "\
<dictionary> {
  ServerAddresses : <array> {
    0 : 2001:db8::1
    1 : fe80::1%en0
  }
}";
        assert_eq!(
            parse_array_field(dump, "ServerAddresses"),
            vec!["2001:db8::1".to_string(), "fe80::1%en0".to_string()]
        );
    }

    #[test]
    fn parses_primary_interface_scalar() {
        assert_eq!(
            parse_scalar_field(GLOBAL_IPV4, "PrimaryInterface"),
            Some("en0".to_string())
        );
    }

    #[test]
    fn scalar_field_absent_yields_none() {
        assert_eq!(parse_scalar_field(GLOBAL_DNS, "PrimaryInterface"), None);
        assert_eq!(parse_scalar_field("", "PrimaryInterface"), None);
    }

    #[test]
    fn scalar_field_does_not_misread_an_array_opener() {
        // `ServerAddresses : <array> {` must not parse as the scalar "<array> {".
        assert_eq!(parse_scalar_field(GLOBAL_DNS, "ServerAddresses"), None);
    }

    // --- the structural decision (per-service model) -------------------------

    fn svc(id: &str, iface: Option<&str>, servers: &[&str]) -> ServiceDns {
        ServiceDns {
            service_id: id.to_string(),
            interface_name: iface.map(str::to_string),
            servers: servers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Build a model from a primary interface, the primary service id, and the
    /// service entries.
    fn model(
        primary_iface: Option<&str>,
        primary_svc: Option<&str>,
        services: Vec<ServiceDns>,
    ) -> DnsModel {
        DnsModel {
            primary_interface: primary_iface.map(str::to_string),
            primary_service: primary_svc.map(str::to_string),
            services,
        }
    }

    #[test]
    fn detects_up_when_a_service_differs_from_the_physical() {
        // The breaking case: a (VPN) service carries corp DNS that differs from
        // the physical en0 service's DHCP resolver. The physical service is the
        // primary service (id "phys").
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]), // physical DHCP
                svc("vpn", Some("en0"), &["192.0.2.53"]),    // VPN's own service (corp)
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn detection_survives_our_own_demote_of_the_physical_service() {
        // After we demote, the PHYSICAL service's DNS is overwritten to the
        // fallback (== its own DHCP resolver here). Detection must still see the
        // VPN via its separate service entry — NOT flip to Down — so there is no
        // oscillation. The physical service still reads 198.51.100.1; the VPN
        // service still reads 192.0.2.53 → still Up, same verdict.
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]), // physical (== demoted value)
                svc("vpn", Some("en0"), &["192.0.2.53"]),    // VPN service unchanged
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn physical_is_anchored_by_service_id_even_if_a_vpn_service_shares_the_interface() {
        // The deeper P1 concern: a VPN service also reports the primary interface
        // name. Anchoring the physical service on the primary SERVICE id (not the
        // interface name) means the VPN service — listed FIRST here — is not
        // mistaken for physical, so corp/fallback are never swapped.
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("vpn", Some("en0"), &["192.0.2.53"]), // VPN, also on en0, listed first
                svc("phys", Some("en0"), &["198.51.100.1"]), // the real physical service
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn falls_back_to_interface_name_when_primary_service_is_unknown() {
        // If PrimaryService is absent, anchor on the primary interface name.
        let m = model(
            Some("en0"),
            None,
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]),
                svc("vpn", Some("en0"), &["192.0.2.53"]),
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn detects_down_when_only_the_physical_service_exists() {
        // No VPN: the only service with DNS is the physical interface's own.
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![svc("phys", Some("en0"), &["198.51.100.1"])],
        );
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn detects_down_when_offline_no_primary() {
        let m = model(
            None,
            None,
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]),
                svc("vpn", Some("en0"), &["192.0.2.53"]),
            ],
        );
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn detects_down_when_no_physical_service_dns_found() {
        // The physical service carries no DNS (cannot pick a safe demote-target),
        // even though some other service has DNS → conservative Down.
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &[]),              // physical, no DNS
                svc("other", Some("en0"), &["192.0.2.53"]), // some other service
            ],
        );
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn decision_is_order_insensitive() {
        // A service whose DNS is the SAME set as physical (just reordered) is not
        // a VPN → Down (no service differs from the physical resolver).
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &["198.51.100.1", "198.51.100.2"]),
                svc("other", Some("en0"), &["198.51.100.2", "198.51.100.1"]),
            ],
        );
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn a_tunnel_vpn_is_detected_by_its_differing_dns_not_its_index() {
        // No SPECIFIC utun index/name is keyed on: a tunnel VPN is recognised by
        // its differing DNS, and its interface only has to be a tunnel *kind*
        // (utun/ppp/ipsec/…) — never a particular name — to qualify as a hijacker.
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]),
                // Whatever the tunnel interface is named (utun index varies), it
                // is recognised as a tunnel kind carrying differing DNS.
                svc("vpn", Some("utun7"), &["192.0.2.53"]),
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn a_secondary_physical_network_is_not_mistaken_for_a_vpn() {
        // The P1 false positive: Ethernet en0 is primary while Wi-Fi en1 stays
        // associated with its own DHCP resolver that differs from en0's. With NO
        // VPN running, en1 is a distinct *hardware* interface (not the primary,
        // not a tunnel) → a parallel network, not the default-resolver hijacker →
        // Down (no false apply of the corp domains to the Wi-Fi resolver).
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]),
                svc("wifi", Some("en1"), &["198.51.100.99"]), // secondary DHCP DNS
            ],
        );
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn a_tunnel_vpn_is_found_even_alongside_a_secondary_physical_network() {
        // A real VPN (utun) AND a secondary physical Wi-Fi are both present, with
        // the secondary listed FIRST. The secondary en1 is excluded; the tunnel
        // hijacker is still found → Up with the VPN's corp DNS (order-insensitive).
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("wifi", Some("en1"), &["198.51.100.99"]), // secondary, listed first
                svc("phys", Some("en0"), &["198.51.100.1"]),
                svc("vpn", Some("utun4"), &["192.0.2.53"]),
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn an_unscoped_hijacker_with_no_interface_is_detected() {
        // A global-default-hijack VPN whose own service entry carries no
        // InterfaceName is an unscoped/default resolver → still a hijacker → Up.
        let m = model(
            Some("en0"),
            Some("phys"),
            vec![
                svc("phys", Some("en0"), &["198.51.100.1"]),
                ServiceDns {
                    service_id: "vpn".to_string(),
                    interface_name: None,
                    servers: vec!["192.0.2.53".to_string()],
                },
            ],
        );
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }

    #[test]
    fn hijacker_predicate_classifies_by_interface_kind() {
        // Primary interface en0. Hijackers: the primary itself, any tunnel kind,
        // or no interface. Non-hijackers: a distinct secondary hardware link.
        let primary = Some("en0");
        let h = |iface: Option<&str>| {
            is_default_resolver_hijacker(&svc("s", iface, &["192.0.2.53"]), primary)
        };
        assert!(h(Some("en0")), "rides the primary default route");
        assert!(
            h(Some("utun4")) && h(Some("ppp0")) && h(Some("ipsec0")),
            "tunnel kinds"
        );
        assert!(h(None), "unscoped/default resolver");
        assert!(
            !h(Some("en1")),
            "a second Ethernet/Wi-Fi is a secondary network"
        );
        assert!(
            !h(Some("bridge100")),
            "a VM/Thunderbolt bridge is a secondary network"
        );
        assert!(!h(Some("pdp_ip0")), "cellular is a secondary network");
    }

    #[test]
    fn full_fixture_parse_then_decide_is_up() {
        // End-to-end over the real-form dumps: parse the primary interface +
        // service id and the two service DNS dicts, assemble the per-service
        // model, decide → Up with corp=192.0.2.53, demote=198.51.100.1.
        let primary_iface = parse_scalar_field(GLOBAL_IPV4, "PrimaryInterface");
        let primary_svc = parse_scalar_field(GLOBAL_IPV4, "PrimaryService");
        let physical = parse_array_field(PHYSICAL_DNS, "ServerAddresses");
        // The VPN's own service DNS dump (corp resolver), same form as the others.
        let vpn_service_dns = "\
<dictionary> {
  ServerAddresses : <array> {
    0 : 192.0.2.53
  }
  InterfaceName : en0
}";
        let corp = parse_array_field(vpn_service_dns, "ServerAddresses");
        let phys_id = primary_svc.clone().unwrap();
        let m = DnsModel {
            primary_interface: primary_iface,
            primary_service: primary_svc,
            services: vec![
                ServiceDns {
                    service_id: phys_id,
                    interface_name: Some("en0".to_string()),
                    servers: physical,
                },
                ServiceDns {
                    service_id: "vpn-service".to_string(),
                    interface_name: Some("en0".to_string()),
                    servers: corp,
                },
            ],
        };
        assert_eq!(
            decide(&m),
            Detected::Up {
                corp_dns: vec!["192.0.2.53".to_string()],
                demote_target: vec!["198.51.100.1".to_string()],
            }
        );
    }
}
