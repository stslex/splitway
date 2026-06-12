#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

use splitway_shared::platform::VpnDetector;

pub fn create_vpn_detector() -> Box<dyn VpnDetector> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxDetector)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosDetector)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsDetector)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        compile_error!("unsupported platform")
    }
}
