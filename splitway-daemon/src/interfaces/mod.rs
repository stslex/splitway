//! Read-only enumeration of the host's network interfaces, for the
//! `ListInterfaces` IPC verb (so a client can offer an interface picker without
//! touching the platform or holding privileges).
//!
//! The split mirrors the detectors': a **thin, untested** per-platform
//! enumeration (`/sys/class/net` on Linux, `getifaddrs` on macOS) feeds a
//! **pure, unit-tested** classification + sort + dedup. Enumeration errors are
//! returned (never panic), so a failure surfaces as `Response::Error` and the
//! GUI falls back to free-text entry.

use splitway_shared::ipc::InterfaceInfo;
use splitway_shared::platform::PlatformError;

/// Interface name prefixes treated as VPN-like. Advisory only — used by a client
/// to sort/highlight, never to filter (the daemon returns every interface).
const VPN_LIKE_PREFIXES: &[&str] = &["tun", "utun", "wg", "tap", "ppp", "gpd"];

/// Whether `name` looks like a VPN interface, by name prefix. Pure heuristic.
fn is_vpn_like(name: &str) -> bool {
    VPN_LIKE_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Turn a raw `(name, up)` enumeration — which may list an interface more than
/// once (`getifaddrs` yields one entry per address family) — into the sorted,
/// de-duplicated, classified list sent over IPC.
///
/// Dedup: by name, OR-ing the `up` flag so an interface counts as up if any of
/// its entries is. Order: up first, then VPN-like first, then name ascending
/// (a stable, deterministic presentation; the client may re-sort). Loopback is
/// included (never filtered) and is simply not VPN-like.
fn classify_and_sort(raw: Vec<(String, bool)>) -> Vec<InterfaceInfo> {
    use std::collections::BTreeMap;

    // BTreeMap dedups by name and gives a deterministic starting order.
    let mut by_name: BTreeMap<String, bool> = BTreeMap::new();
    for (name, up) in raw {
        let entry = by_name.entry(name).or_insert(false);
        *entry = *entry || up;
    }

    let mut interfaces: Vec<InterfaceInfo> = by_name
        .into_iter()
        .map(|(name, up)| {
            let vpn_like = is_vpn_like(&name);
            InterfaceInfo { name, up, vpn_like }
        })
        .collect();

    interfaces.sort_by(|a, b| {
        // `true` should come first, so compare `b` to `a` for the bool keys.
        b.up.cmp(&a.up)
            .then_with(|| b.vpn_like.cmp(&a.vpn_like))
            .then_with(|| a.name.cmp(&b.name))
    });
    interfaces
}

/// Enumerate the host's interfaces: thin platform I/O + the pure pipeline above.
pub fn list_interfaces() -> Result<Vec<InterfaceInfo>, PlatformError> {
    Ok(classify_and_sort(enumerate()?))
}

/// Linux: read `/sys/class/net`. "Up" is the `IFF_UP` admin-up bit from each
/// interface's `flags` file — more reliable than `operstate`, which reports
/// `unknown` for the very virtual links (tun/wg) we most care about.
#[cfg(target_os = "linux")]
fn enumerate() -> Result<Vec<(String, bool)>, PlatformError> {
    let mut raw = Vec::new();
    for entry in std::fs::read_dir("/sys/class/net")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // A missing/unreadable flags file just means "unknown" -> treat as down,
        // rather than failing the whole enumeration for one odd interface.
        let up = std::fs::read_to_string(entry.path().join("flags"))
            .ok()
            .and_then(|flags| parse_iff_up(&flags))
            .unwrap_or(false);
        raw.push((name, up));
    }
    Ok(raw)
}

/// Parse a `/sys/class/net/<n>/flags` value (hex bitmask, e.g. `0x1003`) and
/// return whether the `IFF_UP` (bit 0) flag is set. Pure, so it is unit-tested.
#[cfg(target_os = "linux")]
fn parse_iff_up(flags: &str) -> Option<bool> {
    let trimmed = flags.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u64::from_str_radix(hex, 16)
        .ok()
        .map(|bits| bits & 0x1 != 0)
}

/// macOS: `getifaddrs` yields a linked list with one node per interface address;
/// `classify_and_sort` dedups the repeats by name. "Up" is the `IFF_UP` flag.
#[cfg(target_os = "macos")]
fn enumerate() -> Result<Vec<(String, bool)>, PlatformError> {
    use std::ffi::CStr;

    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` allocates a linked list into `ifap`; we walk it and
    // free it with `freeifaddrs` below.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return Err(PlatformError::Io(std::io::Error::last_os_error()));
    }

    let mut raw = Vec::new();
    let mut cursor = ifap;
    while !cursor.is_null() {
        // SAFETY: `cursor` is non-null and points to a valid `ifaddrs` node
        // owned by the list `getifaddrs` allocated.
        let ifa = unsafe { &*cursor };
        if !ifa.ifa_name.is_null() {
            // SAFETY: `ifa_name` is a valid NUL-terminated C string for the
            // lifetime of the list.
            let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                .to_string_lossy()
                .into_owned();
            let up = ifa.ifa_flags & (libc::IFF_UP as u32) != 0;
            raw.push((name, up));
        }
        cursor = ifa.ifa_next;
    }

    // SAFETY: `ifap` was allocated by `getifaddrs` and has not been freed yet.
    unsafe { libc::freeifaddrs(ifap) };
    Ok(raw)
}

/// Any other Unix (not a CI target): no enumeration source wired up. Return an
/// empty list rather than failing, so the verb stays well-defined.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn enumerate() -> Result<Vec<(String, bool)>, PlatformError> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpn_like_matches_known_prefixes_only() {
        for name in ["tun0", "utun4", "wg0", "tap1", "ppp0", "gpd0"] {
            assert!(is_vpn_like(name), "{name} should be VPN-like");
        }
        for name in ["eth0", "enp3s0", "wlan0", "lo", "docker0", "br0"] {
            assert!(!is_vpn_like(name), "{name} should not be VPN-like");
        }
    }

    #[test]
    fn classify_dedups_by_name_oring_up() {
        // getifaddrs-style repeats: the same interface appears several times,
        // up if any entry is up.
        let raw = vec![
            ("en0".to_string(), false),
            ("en0".to_string(), true), // a later entry is up -> en0 is up
            ("en0".to_string(), false),
        ];
        let out = classify_and_sort(raw);
        assert_eq!(out.len(), 1, "repeats must collapse to one interface");
        assert_eq!(out[0].name, "en0");
        assert!(out[0].up, "up if any entry is up");
    }

    #[test]
    fn classify_sorts_up_first_then_vpn_like_then_name() {
        let raw = vec![
            ("lo".to_string(), true),    // up, not vpn-like
            ("eth0".to_string(), false), // down, not vpn-like
            ("tun0".to_string(), true),  // up, vpn-like
            ("wg1".to_string(), true),   // up, vpn-like
            ("tun9".to_string(), false), // down, vpn-like
            ("eth0".to_string(), false), // duplicate of the down eth0
        ];
        let out = classify_and_sort(raw);
        let order: Vec<&str> = out.iter().map(|i| i.name.as_str()).collect();
        // up+vpn-like (tun0, wg1 by name) -> up+other (lo) -> down+vpn-like
        // (tun9) -> down+other (eth0).
        assert_eq!(order, vec!["tun0", "wg1", "lo", "tun9", "eth0"]);

        // Spot-check the classification flags survived the sort.
        let tun0 = &out[0];
        assert_eq!(tun0.name, "tun0");
        assert!(tun0.up && tun0.vpn_like);
        let eth0 = out.last().unwrap();
        assert_eq!(eth0.name, "eth0");
        assert!(!eth0.up && !eth0.vpn_like);
    }

    #[test]
    fn classify_keeps_loopback_and_never_filters() {
        let raw = vec![("lo".to_string(), true), ("tun0".to_string(), true)];
        let out = classify_and_sort(raw);
        assert!(out.iter().any(|i| i.name == "lo" && !i.vpn_like));
        assert_eq!(out.len(), 2, "nothing is filtered out");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_iff_up_reads_bit_zero() {
        assert_eq!(parse_iff_up("0x1003\n"), Some(true)); // IFF_UP set
        assert_eq!(parse_iff_up("0x1002"), Some(false)); // IFF_UP clear
        assert_eq!(parse_iff_up("4099"), Some(true)); // bare hex (no 0x) = 0x1003
        assert_eq!(parse_iff_up("nothex"), None);
    }
}
