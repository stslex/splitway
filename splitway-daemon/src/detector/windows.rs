use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

pub struct WindowsDetector;

impl VpnDetector for WindowsDetector {
    fn detect(&self, _interface: &str) -> Result<VpnInfo, PlatformError> {
        todo!("windows detect not implemented")
    }

    fn watch(
        &self,
        _interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        todo!("windows watch not implemented")
    }
}
