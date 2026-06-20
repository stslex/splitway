use std::path::Path;
use std::process::Command;

use splitway_shared::ipc::ResolutionInfo;
use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

use crate::backend::linux::query::parse_resolvectl_query;
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

        let result = Command::new("resolvectl")
            .arg("dns")
            .arg(&vpn_info.interface_name)
            .args(&vpn_info.dns_servers)
            .output()?;

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

        // Set domains: resolvectl domain <interface> <domains...>
        let domain_error = match Command::new("resolvectl")
            .arg("domain")
            .arg(&vpn_info.interface_name)
            .args(domains)
            .output()
        {
            Ok(result) => {
                log::debug!(
                    "resolvectl domain stdout: {}",
                    String::from_utf8_lossy(&result.stdout)
                );
                log::debug!(
                    "resolvectl domain stderr: {}",
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
            Err(e) => Some(PlatformError::Io(e)),
        };

        // The DNS step already succeeded, so a domain failure leaves the
        // system half-configured; revert before returning the original error.
        if let Some(error) = domain_error {
            log::error!(
                "domain step failed for {}: {error}; rolling back DNS settings",
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
        let result = Command::new("resolvectl")
            .arg("revert")
            .arg(interface)
            .output()?;

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

    fn status(&self, interface: &str) -> Result<(), PlatformError> {
        let status = Command::new("resolvectl")
            .arg("status")
            .arg(interface)
            .status()?;

        if !status.success() {
            return Err(PlatformError::CommandFailed(
                "resolvectl status failed".to_string(),
            ));
        }

        Ok(())
    }

    /// Strong attribution via systemd-resolved: `resolvectl query` routes the
    /// query by the per-link routing domains, so the link it reports as having
    /// answered reflects the actual split. The resolver IP is not reported, so
    /// `via_dns` stays `None`. This reports which resolver answered, not
    /// reachability (see the trait doc / `docs/architecture.md`).
    fn resolve(&self, host: &str) -> Result<ResolutionInfo, PlatformError> {
        let output = Command::new("resolvectl").arg("query").arg(host).output()?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
