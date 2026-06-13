use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

pub struct MacosDetector;

impl VpnDetector for MacosDetector {
    fn detect(&self, _interface: &str) -> Result<VpnInfo, PlatformError> {
        todo!("macOS detect not implemented")
    }

    fn watch(
        &self,
        _interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        todo!("macOS watch not implemented")
    }
}
