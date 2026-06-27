//! Pure parsing of the macOS SystemConfiguration DNS model — no I/O, unit
//! tested. This is the structural, vendor-neutral core of macOS VPN detection.
//!
//! # Why not `scutil --dns`
//!
//! The original detector filtered `scutil --dns` by a chosen `utun*` interface.
//! That fails against a VPN client that hijacks the system **default** resolver
//! instead of scoping its DNS to the tunnel: there is then *no* resolver scoped
//! to any `utun` (the active tunnel `utun` index even varies between sessions),
//! so an interface-keyed read finds nothing. We instead read the SystemConfig
//! dynamic store directly and decide structurally:
//!
//! - `State:/Network/Global/DNS` `ServerAddresses` — the current system default
//!   resolver(s).
//! - `State:/Network/Global/IPv4` `PrimaryInterface` — the physical primary
//!   interface (e.g. `en0`).
//! - the physical interface's *own* DHCP resolver — the resolver that interface
//!   would use if the default were not overridden.
//!
//! If the global default differs from the physical interface's own DHCP
//! resolver, the default has been overridden by a non-physical (VPN) service →
//! **VPN is up**; the corp DNS is that global default, and the demote-target
//! (where non-corp DNS should go, off-tunnel) is the physical DHCP resolver. The
//! decision keys on this structural *difference*, never on any vendor/product
//! string, so it generalises across VPN clients.

/// One network service's DNS entry, as read from `State:/Network/Service/<id>/DNS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServiceDns {
    /// The `InterfaceName` the service is bound to (e.g. `en0`), if present.
    /// Used only to identify the *physical* service (the one on the primary
    /// interface); a VPN service is identified structurally, not by name.
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
    /// Every network service's DNS entry (from the per-service DNS keys). The
    /// physical service is the one whose `interface_name` is the primary
    /// interface; a VPN service is one whose DNS differs from the physical
    /// service's (a non-physical resolver is in play).
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
/// the demote-target. A **VPN service** is any *other* service that carries DNS
/// differing from the physical service's. VPN is **up** iff such a service
/// exists, and `corp_dns` is its resolver.
///
/// Edge cases, all → [`Detected::Down`] (no false-positive apply):
/// - no primary interface (offline) — nothing to anchor on;
/// - no physical service DNS found — cannot determine the demote-target, and a
///   demote to nothing is worse than not applying, so stay conservative;
/// - no non-physical service whose DNS differs from the physical — no VPN.
pub(super) fn decide(model: &DnsModel) -> Detected {
    // No primary network at all → nothing is in play.
    let Some(primary) = model.primary_interface.as_deref() else {
        return Detected::Down;
    };

    // The physical service: bound to the primary interface and carrying DNS.
    // Its resolver is the demote-target (where non-corp DNS goes off-tunnel).
    let physical_dns = model
        .services
        .iter()
        .find(|s| s.interface_name.as_deref() == Some(primary) && !s.servers.is_empty())
        .map(|s| &s.servers);
    let Some(physical_dns) = physical_dns else {
        // Without the physical resolver we cannot pick a safe demote-target.
        return Detected::Down;
    };

    // A VPN service: any service whose DNS differs from the physical resolver
    // (a non-physical resolver is in play). This signal is independent of the
    // global default, so it survives our own demote of the physical service.
    let vpn_dns = model
        .services
        .iter()
        .filter(|s| !s.servers.is_empty())
        .find(|s| !same_set(&s.servers, physical_dns))
        .map(|s| &s.servers);

    match vpn_dns {
        Some(corp_dns) => Detected::Up {
            corp_dns: corp_dns.clone(),
            demote_target: physical_dns.clone(),
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

    fn svc(iface: Option<&str>, servers: &[&str]) -> ServiceDns {
        ServiceDns {
            interface_name: iface.map(str::to_string),
            servers: servers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Build a model from a primary interface and a list of (iface, servers)
    /// service entries.
    fn model(primary: Option<&str>, services: Vec<ServiceDns>) -> DnsModel {
        DnsModel {
            primary_interface: primary.map(str::to_string),
            services,
        }
    }

    #[test]
    fn detects_up_when_a_service_differs_from_the_physical() {
        // The breaking case: a (VPN) service carries corp DNS that differs from
        // the physical en0 service's DHCP resolver.
        let m = model(
            Some("en0"),
            vec![
                svc(Some("en0"), &["198.51.100.1"]), // physical DHCP
                svc(Some("en0"), &["192.0.2.53"]),   // VPN's own service (corp)
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
            vec![
                svc(Some("en0"), &["198.51.100.1"]), // physical (== demoted value)
                svc(Some("en0"), &["192.0.2.53"]),   // VPN service unchanged
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
        let m = model(Some("en0"), vec![svc(Some("en0"), &["198.51.100.1"])]);
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn detects_down_when_offline_no_primary() {
        let m = model(
            None,
            vec![
                svc(Some("en0"), &["198.51.100.1"]),
                svc(Some("en0"), &["192.0.2.53"]),
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
            vec![
                svc(Some("en0"), &[]),             // physical, no DNS
                svc(Some("en0"), &["192.0.2.53"]), // some other service
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
            vec![
                svc(Some("en0"), &["198.51.100.1", "198.51.100.2"]),
                svc(Some("en0"), &["198.51.100.2", "198.51.100.1"]),
            ],
        );
        assert_eq!(decide(&m), Detected::Down);
    }

    #[test]
    fn decision_ignores_utun_interfaces_entirely() {
        // Proof no `utun` is keyed on: the decision compares per-service DNS
        // against the physical resolver, never a utun name. utun services with
        // their own DNS that differs from physical still simply read as "a VPN
        // service" structurally — by their DNS, not their name.
        let m = model(
            Some("en0"),
            vec![
                svc(Some("en0"), &["198.51.100.1"]),
                // Whatever the tunnel interface is named (utun index varies), it
                // is recognised by its differing DNS, not its name.
                svc(Some("utun7"), &["192.0.2.53"]),
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
    fn full_fixture_parse_then_decide_is_up() {
        // End-to-end over the real-form dumps: parse the primary interface and
        // the two service DNS dicts, assemble the per-service model, decide → Up
        // with corp=192.0.2.53, demote=198.51.100.1.
        let primary = parse_scalar_field(GLOBAL_IPV4, "PrimaryInterface");
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
        let m = DnsModel {
            primary_interface: primary,
            services: vec![
                ServiceDns {
                    interface_name: Some("en0".to_string()),
                    servers: physical,
                },
                ServiceDns {
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
