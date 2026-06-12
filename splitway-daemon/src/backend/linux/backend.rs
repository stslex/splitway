use std::process::Command;

use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

use crate::backend::linux::parser::parse_dns_from_nmcli;
use crate::backend::linux::LinuxBackend;

impl DnsBackend for LinuxBackend {
    fn detect_vpn(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
        let output = Command::new("nmcli")
            .args(["device", "show", interface])
            .output()?;

        if !output.status.success() {
            return Err(PlatformError::VpnNotFound(interface.to_string()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let dns_servers = parse_dns_from_nmcli(&stdout)?;

        Ok(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers,
        })
    }

    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError> {
        // Set DNS servers: resolvectl dns <interface> <servers...>
        if vpn_info.dns_servers.is_empty() {
            return Err(PlatformError::CommandFailed(
                "no DNS servers in VpnInfo".to_string(),
            ));
        }

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
}
