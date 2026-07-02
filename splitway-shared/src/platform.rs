use thiserror::Error;

use crate::ipc::{LinkDnsState, ResolutionInfo};

#[derive(Error, Debug)]
pub enum PlatformError {
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("VPN interface not found: {0}")]
    VpnNotFound(String),
    #[error("failed to parse output: {0}")]
    ParseError(String),
    #[error("D-Bus error: {0}")]
    DbusError(String),
    /// The operation is not supported on this platform (e.g. live resolution on
    /// Windows). A clean, expected error — not a failure.
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct VpnInfo {
    pub interface_name: String,
    pub dns_servers: Vec<String>,
    /// The resolver to fall back to for *non*-routed (off-tunnel) DNS when the
    /// VPN client has hijacked the system **default** resolver rather than
    /// scoping its DNS to the tunnel interface (observed on macOS: the corp
    /// resolver is registered as the global default, so every query — not just
    /// corp ones — would otherwise traverse the tunnel).
    ///
    /// `Some(servers)` means the backend must, in addition to scoping the corp
    /// domains, **demote** the system default to these servers so non-corp DNS
    /// resolves off-tunnel. Typically the physical primary interface's own DHCP
    /// resolver, surfaced by the detector that found the default was overridden.
    ///
    /// `None` (the Linux case, and macOS when no default override is detected)
    /// means scope-only: the platform already keeps the default off-tunnel, so
    /// no demote is needed. Additive — backends that ignore it are unaffected.
    pub demote_target: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub enum VpnEvent {
    Up(VpnInfo),
    Down { interface_name: String },
}

pub trait VpnDetector: Send + Sync {
    /// One-shot detection of the VPN on the given interface.
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError>;

    /// Subscribe to up/down events for the given interface.
    /// The detector owns the background task feeding the channel.
    fn watch(
        &self,
        interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError>;
}

pub trait DnsBackend: Send + Sync {
    /// Apply DNS rules: set DNS servers and route domains through the VPN interface.
    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError>;

    /// Revert DNS rules for the given interface.
    fn revert_rules(&self, interface: &str) -> Result<(), PlatformError>;

    /// Read the **live** per-link DNS state back from the system: the resolvers
    /// and routing domains the link actually has right now. This is the
    /// `reality` read that backs the `Verify` drift-check — distinct from the
    /// daemon's `belief` (its `applied` snapshot).
    ///
    /// Linux parses `resolvectl status <iface>`. macOS is best-effort: there is
    /// no per-link DNS block, so it reconstructs the state from the managed
    /// `/etc/resolver/<domain>` files Splitway wrote (keyed by domain, not
    /// interface — the `interface` argument is therefore advisory there).
    ///
    /// Default: [`PlatformError::Unsupported`] — so a platform without a real
    /// implementation (e.g. Windows) returns a clean error the caller turns into
    /// "read-back unavailable", never a hard failure. A vanished link or a
    /// transient command failure is likewise a clean error, degraded to an empty
    /// read-back by the caller rather than surfaced as an IPC `Error`.
    ///
    /// Boundary: this reports the link's *resolver state*, not reachability —
    /// Splitway governs DNS, not IP routing (see `docs/architecture.md`).
    fn read_link_state(&self, _interface: &str) -> Result<LinkDnsState, PlatformError> {
        Err(PlatformError::Unsupported(
            "DNS read-back is not supported on this platform".to_string(),
        ))
    }

    /// Whether [`Self::revert_rules`] ignores its `interface` argument and
    /// reverts *all* managed DNS state at once, rather than only the named
    /// interface's. macOS is global (it removes every managed `/etc/resolver`
    /// file, which are keyed by domain, not interface); Linux reverts per link.
    ///
    /// The state machine uses this to decide whether it may track and later
    /// clean a single interface orphaned by a failed live switch: on a
    /// global-revert backend that cleanup would also tear down the
    /// currently-applied interface's rules, so orphan tracking is suppressed —
    /// the next apply overwrites the shared state and any later revert is global
    /// anyway.
    fn reverts_globally(&self) -> bool {
        false
    }

    /// Resolve `host` live and report the resolved address(es) plus best-effort
    /// attribution of the link/resolver that answered. This is the route-check's
    /// *reality* read: Linux (systemd-resolved) attributes the answering link
    /// strongly via `resolvectl query`; macOS is best-effort (no attribution).
    ///
    /// Default: [`PlatformError::Unsupported`] — so a platform without a real
    /// implementation (e.g. Windows) returns a clean error that the caller turns
    /// into "resolution unavailable", never a hard failure.
    ///
    /// Boundary: this reports *which resolver answered*, not reachability —
    /// Splitway governs DNS, not IP routing (see `docs/architecture.md`).
    fn resolve(&self, _host: &str) -> Result<ResolutionInfo, PlatformError> {
        Err(PlatformError::Unsupported(
            "live resolution is not supported on this platform".to_string(),
        ))
    }
}
