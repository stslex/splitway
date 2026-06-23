use std::path::Path;

use splitway_shared::ipc::{LinkDnsState, ResolutionInfo};
use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

use crate::backend::linux::query::parse_resolvectl_query;
use crate::backend::linux::status::parse_resolvectl_status;
use crate::backend::linux::LinuxBackend;

impl DnsBackend for LinuxBackend {
    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError> {
        // No pushed DNS (e.g. a standalone OpenVPN whose PUSH_REPLY carried no
        // `dhcp-option DNS`): there is nothing to route queries to, so calling
        // `resolvectl dns <iface>` with zero servers — or applying routing
        // domains that point at a link with no resolver — would leave a broken,
        // half-configured rule. Treat it as a successful no-op and log instead.
        //
        // The state machine's `desired()` already gates this out (an Up with no
        // DNS reverts/no-ops rather than applying), so this branch is normally
        // unreached from the daemon; it stays as defense-in-depth for any direct
        // caller so a zero-server apply can never half-configure the link.
        if vpn_info.dns_servers.is_empty() {
            log::info!(
                "{}: VPN up but no DNS servers were provided; leaving DNS unchanged \
                 (no split-DNS to apply)",
                vpn_info.interface_name
            );
            return Ok(());
        }

        // Set DNS servers: resolvectl dns <interface> <servers...>

        let result = crate::exec::run(
            crate::exec::tool("SPLITWAY_RESOLVECTL", "resolvectl")
                .arg("dns")
                .arg(&vpn_info.interface_name)
                .args(&vpn_info.dns_servers),
            "resolvectl",
            "split-DNS apply",
        )?;

        log::debug!(
            "resolvectl dns stdout: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        log::debug!(
            "resolvectl dns stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );

        if !result.status.success() {
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&result.stderr).to_string(),
            ));
        }

        // Set domains as systemd-resolved **routing-only** (`~domain`): route
        // `*.domain` queries to this link without polluting the search list.
        // `resolvectl domain` replaces the link's whole domain list, so this also
        // drops any catch-all/search domains a VPN client left on the link.
        let routing_domains = routing_only(domains);
        let mut step_error = run_apply_step(
            crate::exec::tool("SPLITWAY_RESOLVECTL", "resolvectl")
                .arg("domain")
                .arg(&vpn_info.interface_name)
                .args(&routing_domains),
            "domain",
        );

        // Disable the link's DNS default-route (catch-all) flag so it resolves
        // ONLY its routing domains. A full-tunnel VPN carries the default IP route,
        // which makes systemd-resolved mark the link as the DNS *default route* —
        // so without this every name that matches no routing domain (a sibling of a
        // configured domain, or anything else) leaks to the VPN resolver, defeating
        // the split. Run last: if it fails the link still has correct servers and
        // domains (no worse than before this step), and the rollback below restores
        // a clean state. `resolvectl revert` clears this flag along with the servers
        // and domains, so the rollback path needs no extra step.
        if step_error.is_none() {
            step_error = run_apply_step(
                crate::exec::tool("SPLITWAY_RESOLVECTL", "resolvectl")
                    .arg("default-route")
                    .arg(&vpn_info.interface_name)
                    .arg("false"),
                "default-route",
            );
        }

        // The DNS step already succeeded, so a later step's failure leaves the
        // system half-configured; revert before returning the original error.
        if let Some(error) = step_error {
            log::error!(
                "split-DNS apply step failed for {}: {error}; rolling back DNS settings",
                vpn_info.interface_name
            );
            match self.revert_rules(&vpn_info.interface_name) {
                Ok(()) => log::info!(
                    "rollback succeeded: {} restored to its pre-apply state",
                    vpn_info.interface_name
                ),
                Err(revert_error) => log::error!(
                    "rollback failed for {}: {revert_error}; system may be half-configured",
                    vpn_info.interface_name
                ),
            }
            return Err(error);
        }

        Ok(())
    }

    fn revert_rules(&self, interface: &str) -> Result<(), PlatformError> {
        let result = crate::exec::run(
            crate::exec::tool("SPLITWAY_RESOLVECTL", "resolvectl")
                .arg("revert")
                .arg(interface),
            "resolvectl",
            "DNS revert",
        )?;

        log::debug!(
            "resolvectl revert stdout: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        log::debug!(
            "resolvectl revert stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );

        if !result.status.success() {
            // A revert can fail simply because the link has already vanished
            // (the common VPN down/remove race): systemd-resolved drops a link's
            // DNS settings when the link disappears, so there is nothing left to
            // revert and the system is already in the desired (clean) state.
            // Treat a now-absent interface as success; only a failure with the
            // link still present is a real error worth surfacing.
            if !interface_exists(interface) {
                log::debug!(
                    "resolvectl revert {interface} failed, but the link is gone; \
                     treating as already reverted"
                );
                return Ok(());
            }
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&result.stderr).to_string(),
            ));
        }

        Ok(())
    }

    /// Read the live per-link DNS state from `resolvectl status <iface>` and
    /// parse it (I/O-free) via [`parse_resolvectl_status`]. A non-zero exit or a
    /// vanished link is a clean [`PlatformError`] the daemon degrades to
    /// "read-back unavailable" — never a hard failure. This reports the link's
    /// resolver state, not reachability (see the trait doc / `docs/architecture.md`).
    fn read_link_state(&self, interface: &str) -> Result<LinkDnsState, PlatformError> {
        let output = crate::exec::run(
            crate::exec::tool("SPLITWAY_RESOLVECTL", "resolvectl")
                .arg("status")
                .arg(interface),
            "resolvectl",
            "DNS read-back",
        )?;

        log::debug!(
            "resolvectl status stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        if !output.status.success() {
            // A non-zero exit is usually a vanished link (the VPN-down race) or a
            // bad interface name; surface it as a clean error the daemon turns
            // into "read-back unavailable".
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }

        Ok(parse_resolvectl_status(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    /// Strong attribution via systemd-resolved: `resolvectl query` routes the
    /// query by the per-link routing domains, so the link it reports as having
    /// answered reflects the actual split. The resolver IP is not reported, so
    /// `via_dns` stays `None`. This reports which resolver answered, not
    /// reachability (see the trait doc / `docs/architecture.md`).
    fn resolve(&self, host: &str) -> Result<ResolutionInfo, PlatformError> {
        let output = crate::exec::run(
            crate::exec::tool("SPLITWAY_RESOLVECTL", "resolvectl")
                .arg("query")
                .arg(host),
            "resolvectl",
            "DNS resolution",
        )?;

        log::debug!(
            "resolvectl query stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        if !output.status.success() {
            // A non-zero exit is the normal NXDOMAIN / SERVFAIL path; surface it
            // as a clean error the daemon turns into "resolution unavailable".
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }

        let info = parse_resolvectl_query(&String::from_utf8_lossy(&output.stdout));
        if info.addresses.is_empty() {
            return Err(PlatformError::ParseError(format!(
                "no addresses parsed from `resolvectl query {host}`"
            )));
        }
        Ok(info)
    }
}

/// Whether a network interface currently exists, via its sysfs entry. Used to
/// treat a failed `resolvectl revert` as success when the link has already
/// vanished — its per-link DNS state is gone with it, so there is nothing left
/// to revert.
fn interface_exists(interface: &str) -> bool {
    Path::new("/sys/class/net").join(interface).exists()
}

/// Map configured (bare) domains to systemd-resolved **routing-only** form by
/// prefixing each with `~`: `jira.example.com` → `~jira.example.com`. A
/// routing-only domain routes `*.domain` queries to the link but is *not* added
/// to the search list — a bare domain is both a search and a routing domain, so a
/// single-label lookup (`host foo`) would otherwise get the domain appended and
/// routed here. A domain already carrying the `~` marker (a hand-edited config) is
/// left unchanged so it is never double-prefixed. Pure, no I/O.
fn routing_only(domains: &[String]) -> Vec<String> {
    domains
        .iter()
        .map(|domain| {
            if domain.starts_with('~') {
                domain.clone()
            } else {
                format!("~{domain}")
            }
        })
        .collect()
}

/// Run one fallible `resolvectl` mutation step of [`apply_rules`], log its output
/// at debug, and map a non-zero exit (or a spawn failure) to a [`PlatformError`].
/// Returns `None` on success. Shared by the domain and default-route steps so both
/// feed the single rollback path identically; `step` names the step for logs.
fn run_apply_step(cmd: &mut std::process::Command, step: &str) -> Option<PlatformError> {
    match crate::exec::run(cmd, "resolvectl", "split-DNS apply") {
        Ok(result) => {
            log::debug!(
                "resolvectl {step} stdout: {}",
                String::from_utf8_lossy(&result.stdout)
            );
            log::debug!(
                "resolvectl {step} stderr: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            if result.status.success() {
                None
            } else {
                Some(PlatformError::CommandFailed(
                    String::from_utf8_lossy(&result.stderr).to_string(),
                ))
            }
        }
        Err(e) => Some(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_only_prefixes_bare_domains_and_preserves_existing_marker() {
        // Bare config domains gain the `~` routing-only marker; a domain already
        // carrying it (a hand-edited config) is not double-prefixed.
        let domains = vec![
            "jira.example.com".to_string(),
            "corp.example.com".to_string(),
            "~already.example.net".to_string(),
        ];
        assert_eq!(
            routing_only(&domains),
            vec![
                "~jira.example.com",
                "~corp.example.com",
                "~already.example.net",
            ]
        );
        // No domains → no args (the empty-DNS path never reaches this anyway).
        assert!(routing_only(&[]).is_empty());
    }

    #[test]
    fn apply_with_no_dns_is_a_noop_ok() {
        // The no-pushed-DNS case must succeed without ever shelling out to
        // resolvectl (the early return happens before any Command), so this
        // passes regardless of whether resolvectl is installed.
        let info = VpnInfo {
            interface_name: "tun0".to_string(),
            dns_servers: Vec::new(),
        };
        let result = LinuxBackend.apply_rules(&info, &["corp.example.com".to_string()]);
        assert!(
            result.is_ok(),
            "empty DNS should be a logged no-op, not an error"
        );
    }
}
