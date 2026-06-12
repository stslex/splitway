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

    /// Spawns the NetworkManager D-Bus watch task on the ambient tokio
    /// runtime. Panics if called outside one; the `watch` subcommand
    /// handler owns the runtime until the daemon goes async in Phase 2.
    fn watch(
        &self,
        interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let interface = interface.to_string();
        tokio::spawn(async move {
            log::debug!("starting NetworkManager watch for {interface}");
            if let Err(e) = super::dbus::watch_loop(interface.clone(), tx).await {
                log::error!("VPN watch for {interface} terminated: {e}");
            }
        });
        Ok(rx)
    }
}
