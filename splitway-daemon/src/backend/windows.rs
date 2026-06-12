use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

pub struct WindowsBackend;

impl DnsBackend for WindowsBackend {
    fn apply_rules(&self, _vpn_info: &VpnInfo, _domains: &[String]) -> Result<(), PlatformError> {
        todo!("windows apply_rules not implemented")
    }

    fn revert_rules(&self, _interface: &str) -> Result<(), PlatformError> {
        todo!("windows revert_rules not implemented")
    }

    fn status(&self, _interface: &str) -> Result<(), PlatformError> {
        todo!("windows status not implemented")
    }
}
