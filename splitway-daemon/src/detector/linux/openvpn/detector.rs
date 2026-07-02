//! Wires the OpenVPN management plumbing to the [`VpnDetector`] trait.

use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

use super::parser::parse_management_addr;
use super::{mgmt, OpenVpnDetector};

impl VpnDetector for OpenVpnDetector {
    /// One-shot probe of the management interface: connect, sample the current
    /// state and any pushed DNS, and return [`VpnInfo`] if the tunnel is up.
    /// `interface` names the `tun*` device the backend will target (from config
    /// `vpn_name`); the management interface supplies the DNS, not the device.
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
        let addr = parse_management_addr(&self.management)?;
        let password = self.read_password()?;
        let (connected, dns_servers) = mgmt::blocking_sample(&addr, password.as_deref())?;
        if !connected {
            return Err(PlatformError::VpnNotFound(interface.to_string()));
        }
        Ok(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers,
            // Linux scopes DNS per link; the default is not hijacked.
            demote_target: None,
        })
    }

    /// Spawn the management watch task on the ambient tokio runtime (mirrors
    /// `LinuxDetector::watch`). A bad management address or unreadable password
    /// file fails here — a clear error the daemon logs while keeping IPC up and
    /// auto-apply off — rather than only inside the spawned task.
    fn watch(
        &self,
        interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            PlatformError::CommandFailed(format!("watch requires a running tokio runtime: {e}"))
        })?;
        let addr = parse_management_addr(&self.management)?;
        let password = self.read_password()?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let interface = interface.to_string();
        handle.spawn(async move {
            log::debug!("starting openvpn management watch for {interface} via {addr}");
            if let Err(e) = mgmt::run(interface.clone(), addr, password, tx).await {
                log::error!("openvpn watch for {interface} terminated: {e}");
            }
        });
        Ok(rx)
    }
}
