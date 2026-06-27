//! Wires the macOS structural DNS-model detection to the [`VpnDetector`] trait.
//!
//! The I/O here is deliberately thin: it shells `scutil` to dump the
//! SystemConfiguration dynamic-store keys, hands the raw text to the pure
//! [`parser`](super::parser) module, and runs the pure structural decision. All
//! the logic (what counts as "VPN up", which resolver is the demote-target) is
//! in the parser and unit-tested without a live system.

use std::process::Command;

use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

use super::parser::{
    decide, parse_array_field, parse_scalar_field, Detected, DnsModel, ServiceDns,
};
use super::MacosDetector;

impl VpnDetector for MacosDetector {
    /// One-shot detection. The `interface` argument is **advisory on macOS**:
    /// detection is driven by the system DNS model (which resolver is the
    /// default vs. the physical interface's own), not by an interface name —
    /// the active VPN tunnel is not reliably a named, DNS-scoped link here.
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
        match current_vpn_state()? {
            Detected::Up {
                corp_dns,
                demote_target,
            } => Ok(VpnInfo {
                // Advisory only — nothing keys on it on macOS. Report the
                // configured name so status output stays recognisable.
                interface_name: interface.to_string(),
                dns_servers: corp_dns,
                demote_target: Some(demote_target),
            }),
            Detected::Down => Err(PlatformError::VpnNotFound(interface.to_string())),
        }
    }

    fn watch(
        &self,
        interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        super::watch::watch(interface)
    }
}

/// Read the system DNS model from the dynamic store and run the structural
/// up/down decision. Shared by [`detect`](MacosDetector::detect) and the
/// SCDynamicStore watch callback.
///
/// `Detected::Down` (not an error) means no VPN-imposed default — the watcher
/// treats it as "down". A genuine `scutil` failure is an `Err` the caller may
/// retry; "no override" is a normal, successful "nothing here".
pub(super) fn current_vpn_state() -> Result<Detected, PlatformError> {
    let model = read_dns_model()?;
    Ok(decide(&model))
}

/// Assemble the [`DnsModel`] from the dynamic store via `scutil`: the primary
/// interface plus every network service's DNS entry. Detection reads the
/// **per-service** entries (not the mutable global default), so Splitway's own
/// demote of the physical service does not make detection flip to "down".
fn read_dns_model() -> Result<DnsModel, PlatformError> {
    let global_ipv4_dump = scutil_show("State:/Network/Global/IPv4")?;
    let primary_interface = parse_scalar_field(&global_ipv4_dump, "PrimaryInterface");

    // Enumerate every per-service DNS key and read its InterfaceName + servers.
    let listing = scutil_list("State:/Network/Service/.*/DNS")?;
    let mut services = Vec::new();
    for key in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let dump = match scutil_show(key) {
            Ok(dump) => dump,
            // A key that vanished between list and show is skipped, not fatal.
            Err(e) => {
                log::debug!("reading {key} failed: {e}; skipping");
                continue;
            }
        };
        let servers = parse_array_field(&dump, "ServerAddresses");
        if servers.is_empty() {
            continue; // a service with no DNS contributes nothing
        }
        services.push(ServiceDns {
            interface_name: parse_scalar_field(&dump, "InterfaceName"),
            servers,
        });
    }

    Ok(DnsModel {
        primary_interface,
        services,
    })
}

/// Run `scutil` with a `show <key>` script over stdin and return the dump. A
/// missing key makes `scutil` print `No such key` (not a process failure); the
/// parser treats that as empty, so it is returned as-is rather than an error.
fn scutil_show(key: &str) -> Result<String, PlatformError> {
    scutil_script(&format!("show {key}\n"))
}

/// Run `scutil` with a `list <pattern>` script and return the matching keys,
/// one per line.
fn scutil_list(pattern: &str) -> Result<String, PlatformError> {
    scutil_script(&format!("list {pattern}\n"))
}

/// Drive `scutil` in script mode by piping `script` (terminated by an implicit
/// quit on EOF) to its stdin. Centralises the spawn + error mapping so the
/// individual readers stay one-liners and the whole I/O surface is one function.
fn scutil_script(script: &str) -> Result<String, PlatformError> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PlatformError::CommandFailed(format!("failed to spawn scutil: {e}")))?;

    // Write the script, then drop stdin so scutil sees EOF and exits.
    {
        let stdin = child.stdin.take().ok_or_else(|| {
            PlatformError::CommandFailed("scutil stdin was not captured".to_string())
        })?;
        let mut stdin = stdin;
        stdin
            .write_all(script.as_bytes())
            .map_err(|e| PlatformError::CommandFailed(format!("writing scutil script: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| PlatformError::CommandFailed(format!("waiting for scutil: {e}")))?;
    if !output.status.success() {
        return Err(PlatformError::CommandFailed(format!(
            "scutil exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
