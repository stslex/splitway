use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

pub struct MacosBackend;

impl DnsBackend for MacosBackend {
    fn detect_vpn(&self, _interface: &str) -> Result<VpnInfo, PlatformError> {
        todo!("macOS detect_vpn not implemented")
    }

    fn apply_rules(&self, _vpn_info: &VpnInfo, _domains: &[String]) -> Result<(), PlatformError> {
        todo!("macOS apply_rules not implemented")
    }

    fn revert_rules(&self, _interface: &str) -> Result<(), PlatformError> {
        todo!("macOS revert_rules not implemented")
    }

    fn status(&self, _interface: &str) -> Result<(), PlatformError> {
        todo!("macOS status not implemented")
    }
}
