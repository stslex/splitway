use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

pub struct WindowsBackend;

impl DnsBackend for WindowsBackend {
    fn apply_rules(&self, _vpn_info: &VpnInfo, _domains: &[String]) -> Result<(), PlatformError> {
        todo!("windows apply_rules not implemented")
    }

    fn revert_rules(&self, _interface: &str) -> Result<(), PlatformError> {
        todo!("windows revert_rules not implemented")
    }

    // `read_link_state` is intentionally not implemented: Windows is unsupported,
    // so it inherits the trait's default clean `PlatformError::Unsupported`, which
    // the daemon degrades to "read-back unavailable".
}
