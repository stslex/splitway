use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlatformError {
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("VPN interface not found: {0}")]
    VpnNotFound(String),
    #[error("failed to parse output: {0}")]
    ParseError(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct VpnInfo {
    pub interface_name: String,
    pub dns_servers: Vec<String>,
}

pub trait DnsBackend: Send + Sync {
    /// Detect VPN on the given interface and return its DNS info.
    fn detect_vpn(&self, interface: &str) -> Result<VpnInfo, PlatformError>;

    /// Apply DNS rules: set DNS servers and route domains through the VPN interface.
    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError>;

    /// Revert DNS rules for the given interface.
    fn revert_rules(&self, interface: &str) -> Result<(), PlatformError>;

    /// Show DNS status for the given interface.
    fn status(&self, interface: &str) -> Result<(), PlatformError>;
}
