#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// Re-export the macOS dynamic-store primitives so the macOS DNS backend can
/// reuse them for the demote (it reads the same `scutil` dump shape to find the
/// primary service and its current DNS, drives `scutil` the same way, and
/// compares resolver sets the same way). Kept here, behind the platform gate, so
/// the two macOS modules share one implementation rather than duplicating it.
#[cfg(target_os = "macos")]
pub(crate) use macos::{
    parse_array_field as macos_parse_array_field, parse_scalar_field as macos_parse_scalar_field,
    same_set as macos_same_set, scutil_script as macos_scutil_script,
};

#[cfg(target_os = "windows")]
mod windows;

use splitway_shared::config::LocalConfig;
use splitway_shared::platform::VpnDetector;

/// Build the platform's VPN detector. On Linux the choice is `config`-driven
/// (`vpn_backend`): NetworkManager (default) or standalone OpenVPN. macOS and
/// Windows have a single detector and ignore the field for now — the selector
/// is shaped so they can adopt it later without rework.
pub fn create_vpn_detector(config: &LocalConfig) -> Box<dyn VpnDetector> {
    // The selector is Linux-only today; other platforms ignore `config`.
    #[cfg(not(target_os = "linux"))]
    let _ = config;

    #[cfg(target_os = "linux")]
    {
        use splitway_shared::config::VpnBackend;
        match config.vpn_backend {
            VpnBackend::NetworkManager => Box::new(linux::LinuxDetector),
            VpnBackend::OpenVpn => Box::new(linux::openvpn::OpenVpnDetector::from_config(
                &config.openvpn,
            )),
        }
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
