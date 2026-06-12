use std::process::Command;

use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

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
        let dns_ip = stdout
            .lines()
            .find(|line| line.contains("DNS"))
            .and_then(|line| line.split_whitespace().last())
            .map(|ip| ip.to_string())
            .ok_or_else(|| {
                PlatformError::ParseError("DNS entry not found in nmcli output".to_string())
            })?;

        Ok(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers: vec![dns_ip],
        })
    }

    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError> {
        // Set DNS server: resolvectl dns <interface> <ip>
        let dns_server = vpn_info
            .dns_servers
            .first()
            .ok_or_else(|| PlatformError::CommandFailed("no DNS servers in VpnInfo".to_string()))?;

        let result = Command::new("/usr/bin/resolvectl")
            .arg("dns")
            .arg(&vpn_info.interface_name)
            .arg(dns_server)
            .output()?;

        println!("stdout: {}", String::from_utf8_lossy(&result.stdout));
        println!("stderr: {}", String::from_utf8_lossy(&result.stderr));

        if !result.status.success() {
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&result.stderr).to_string(),
            ));
        }

        // Set domains: resolvectl domain <interface> <domains...>
        let result = Command::new("/usr/bin/resolvectl")
            .arg("domain")
            .arg(&vpn_info.interface_name)
            .args(domains)
            .output()?;

        println!("stdout: {}", String::from_utf8_lossy(&result.stdout));
        println!("stderr: {}", String::from_utf8_lossy(&result.stderr));

        if !result.status.success() {
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&result.stderr).to_string(),
            ));
        }

        Ok(())
    }

    fn revert_rules(&self, interface: &str) -> Result<(), PlatformError> {
        let result = Command::new("/usr/bin/resolvectl")
            .arg("revert")
            .arg(interface)
            .output()?;

        println!("stdout: {}", String::from_utf8_lossy(&result.stdout));
        println!("stderr: {}", String::from_utf8_lossy(&result.stderr));

        if !result.status.success() {
            return Err(PlatformError::CommandFailed(
                String::from_utf8_lossy(&result.stderr).to_string(),
            ));
        }

        Ok(())
    }

    fn status(&self, interface: &str) -> Result<(), PlatformError> {
        let status = Command::new("/usr/bin/resolvectl")
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
