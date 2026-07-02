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
    let primary_service = parse_scalar_field(&global_ipv4_dump, "PrimaryService");

    // Enumerate every per-service DNS key and read its id + InterfaceName + servers.
    // `scutil`'s `list` does NOT print bare keys: each match is a prefixed row
    // (`subKey [<n>] = State:/.../DNS`), so the key must be parsed out of the row
    // before it is shown — otherwise every `show` is fed the whole row, returns
    // `No such key`, and the model stays empty (detection always "down").
    let listing = scutil_list("State:/Network/Service/.*/DNS")?;
    let mut services = Vec::new();
    for row in listing.lines() {
        let key = parse_list_key(row);
        // Skip blank/header rows and anything outside the listed pattern.
        if !key.starts_with("State:/Network/Service/") {
            continue;
        }
        // Propagate a real `scutil` failure rather than skipping the service: a
        // missing/vanished key is reported as `No such key` on stdout with exit 0
        // (→ `Ok`, then empty servers below), so an `Err` here is a genuine command
        // failure (spawn error or non-zero exit). Skipping it would silently drop a
        // live service from the model — e.g. the VPN service, read after a
        // successful `list` — and `decide` could then conclude "down" and revert the
        // rules. Failing the read instead makes the watcher keep the last known
        // state until the next change.
        let dump = scutil_show(key)?;
        let servers = parse_array_field(&dump, "ServerAddresses");
        if servers.is_empty() {
            continue; // a service with no DNS (incl. the `No such key` case) contributes nothing
        }
        let service_id = service_id_from_key(key);
        // The interface binding is read from the service's IPv4/IPv6 entity, NOT
        // the DNS dict — the DNS schema is the DNS fields (ServerAddresses, …) and
        // does not reliably carry InterfaceName. Reading it only from the DNS dict
        // would leave a secondary Wi-Fi/Ethernet service's interface unknown, and
        // `decide` would then misread that `None` as an unscoped/default hijacker
        // (a false "VPN up"). Fall back to the DNS dict only if neither entity
        // names an interface.
        let interface_name = read_service_interface(&service_id)
            .or_else(|| parse_scalar_field(&dump, "InterfaceName"));
        services.push(ServiceDns {
            service_id,
            interface_name,
            servers,
        });
    }

    Ok(DnsModel {
        primary_interface,
        primary_service,
        services,
    })
}

/// Read the BSD interface a service is bound to, from its IPv4 (then IPv6)
/// dynamic-store entity — the reliable source for the binding. The per-service
/// DNS entity does not reliably carry `InterfaceName` (its schema is the DNS
/// fields), so reading the interface only from the DNS dict can leave a secondary
/// network's interface unknown, which `decide` would misclassify as an unscoped
/// (default-hijacker) resolver. Returns `None` only when neither entity names one.
fn read_service_interface(service_id: &str) -> Option<String> {
    for entity in ["IPv4", "IPv6"] {
        let key = format!("State:/Network/Service/{service_id}/{entity}");
        if let Ok(dump) = scutil_show(&key) {
            if let Some(iface) = parse_scalar_field(&dump, "InterfaceName") {
                return Some(iface);
            }
        }
    }
    None
}

/// Extract the dynamic-store key from one `scutil list` output row. `scutil`
/// prints each match as `  subKey [<n>] = State:/Network/Service/<id>/DNS`, not
/// as a bare key, so the `subKey [n] = ` prefix must be stripped before the key
/// can be passed to `show` / [`service_id_from_key`]. A row without the ` = `
/// marker is returned trimmed (defensive); the caller filters anything that is
/// not a service DNS key.
fn parse_list_key(row: &str) -> &str {
    let row = row.trim();
    match row.rsplit_once(" = ") {
        Some((_, key)) => key.trim(),
        None => row,
    }
}

/// Extract the `<id>` from a `State:/Network/Service/<id>/DNS` key, so it can be
/// matched against `PrimaryService`. Returns the whole key if it does not fit
/// the expected shape (so an unexpected key never silently collides with a real
/// service id).
fn service_id_from_key(key: &str) -> String {
    key.strip_prefix("State:/Network/Service/")
        .and_then(|rest| rest.strip_suffix("/DNS"))
        .unwrap_or(key)
        .to_string()
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
///
/// Shared with the demote backend (`crate::detector::macos_scutil_script`) so the
/// single `scutil` driver — spawn, stdin pipe, exit-status mapping — has exactly
/// one implementation; a future change to it (a timeout, non-UTF-8 handling)
/// lands in one place rather than drifting between two near-identical copies.
/// Returns the raw stdout; a non-zero exit is mapped to `Err`, but note `scutil`
/// in script mode also reports some command failures on stdout with exit 0 (a
/// missing key is `No such key`), so a *set* caller must additionally inspect the
/// returned stdout (see the demote's `RealScutil::run_script`).
pub(crate) fn scutil_script(script: &str) -> Result<String, PlatformError> {
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

#[cfg(test)]
mod tests {
    use super::{parse_list_key, service_id_from_key};

    #[test]
    fn parse_list_key_strips_the_scutil_subkey_prefix() {
        // The real `scutil list` row shape — the key must be parsed out, not shown
        // verbatim (the bug: the whole row went to `show` → `No such key`).
        assert_eq!(
            parse_list_key("  subKey [0] = State:/Network/Service/ABC-123/DNS"),
            "State:/Network/Service/ABC-123/DNS"
        );
        assert_eq!(
            parse_list_key("subKey [12] = State:/Network/Service/XYZ/DNS"),
            "State:/Network/Service/XYZ/DNS"
        );
    }

    #[test]
    fn parse_list_key_passes_a_bare_key_through() {
        // Defensive: a row already in bare-key form is returned trimmed.
        assert_eq!(
            parse_list_key("  State:/Network/Service/ABC/DNS  "),
            "State:/Network/Service/ABC/DNS"
        );
    }

    #[test]
    fn service_id_from_key_extracts_the_id() {
        assert_eq!(
            service_id_from_key("State:/Network/Service/ABC-123/DNS"),
            "ABC-123"
        );
    }

    #[test]
    fn service_id_from_key_returns_whole_key_when_shape_is_unexpected() {
        // An unexpected key shape is returned verbatim, so it never collides with
        // a real service id.
        assert_eq!(
            service_id_from_key("Setup:/Network/Foo"),
            "Setup:/Network/Foo"
        );
        assert_eq!(
            service_id_from_key("State:/Network/Service/ABC"),
            "State:/Network/Service/ABC"
        );
    }
}
