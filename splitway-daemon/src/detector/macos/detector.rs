//! Wires the macOS parser/watch to the [`VpnDetector`] trait.

use std::process::Command;

use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

use super::parser::parse_scutil_dns;
use super::MacosDetector;

impl VpnDetector for MacosDetector {
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
        let dns_servers = current_dns(interface)?;
        if dns_servers.is_empty() {
            return Err(PlatformError::VpnNotFound(interface.to_string()));
        }
        Ok(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers,
        })
    }

    fn watch(
        &self,
        interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        super::watch::watch(interface)
    }
}

/// Read the DNS servers macOS currently associates with `interface` via
/// `scutil --dns`. Returns an empty vec (not an error) when the interface has
/// no resolver, so the watcher can treat that as "down". Shared by `detect`
/// and the SCDynamicStore callback.
pub(super) fn current_dns(interface: &str) -> Result<Vec<String>, PlatformError> {
    let output = Command::new("scutil").arg("--dns").output()?;
    if !output.status.success() {
        return Err(PlatformError::CommandFailed(
            "scutil --dns failed".to_string(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_scutil_dns(&stdout, interface))
}
