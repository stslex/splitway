#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

use splitway_shared::platform::DnsBackend;

pub fn create_backend() -> Box<dyn DnsBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxBackend)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosBackend)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsBackend)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        compile_error!("unsupported platform")
    }
}
