use std::process::Command;

use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

use crate::detector::linux::parser::parse_dns_from_nmcli;
use crate::detector::linux::LinuxDetector;

impl VpnDetector for LinuxDetector {
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
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

    fn watch(
        &self,
        _interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        todo!("NetworkManager D-Bus watch not implemented yet")
    }
}
