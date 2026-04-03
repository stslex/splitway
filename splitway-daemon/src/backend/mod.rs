#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        compile_error!("unsupported platform")
    }
}
