//! Standalone-OpenVPN VPN detector (no NetworkManager).
//!
//! Mirrors the other detectors' thin-plumbing / pure-logic split: [`parser`]
//! (management line parsing) and [`state`] (state-token -> transition mapping,
//! reusing the NM detector's `Transition`/`Deduper`) are pure and unit-tested;
//! [`mgmt`] is the management-socket glue, and [`detector`] wires them to the
//! `VpnDetector` trait.
//!
//! Unlike OpenVPN-over-NetworkManager (phase 3a), nothing applies the pushed
//! DNS to the `tun*` link for us to read back, so the pushed DNS is learned
//! from OpenVPN's own management interface (`log on` surfaces the `PUSH_REPLY`).
//! The `DnsBackend` (resolvectl) is unchanged: it applies per-link DNS on the
//! `tun*` device exactly as it does for NM.

mod detector;
mod mgmt;
mod parser;
mod state;

use std::path::PathBuf;

use splitway_shared::config::OpenVpnConfig;
use splitway_shared::platform::PlatformError;

/// Detects a standalone OpenVPN connection via its management interface.
pub(crate) struct OpenVpnDetector {
    /// Management address as configured (`host:port` or a unix socket path),
    /// parsed lazily so a bad value surfaces as a clear error at `watch`/`detect`.
    management: String,
    /// Optional path to the management password file (first line = password).
    password_file: Option<PathBuf>,
}

impl OpenVpnDetector {
    pub(crate) fn from_config(config: &OpenVpnConfig) -> Self {
        Self {
            management: config.management.clone(),
            password_file: config.management_password_file.clone().map(PathBuf::from),
        }
    }

    /// Read the management password (the file's first line) if a password file
    /// is configured. An unreadable file is a clear error, surfaced to the
    /// caller (daemon logs it and leaves auto-apply off).
    fn read_password(&self) -> Result<Option<String>, PlatformError> {
        let Some(path) = &self.password_file else {
            return Ok(None);
        };
        let contents = std::fs::read_to_string(path).map_err(|e| {
            PlatformError::CommandFailed(format!(
                "failed to read openvpn management password file {}: {e}",
                path.display()
            ))
        })?;
        Ok(Some(contents.lines().next().unwrap_or("").to_string()))
    }
}
