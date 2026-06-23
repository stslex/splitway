use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent, VpnInfo};

use crate::detector::linux::parser::{extract_dns_from_nmcli, parse_active_vpn_uuids};
use crate::detector::linux::LinuxDetector;

/// How many times the DNS fallback chain is read before concluding the VPN
/// pushed no DNS, and the delay between reads.
///
/// NM signals device-state `100` slightly *before* the pushed IP config — and
/// therefore the active-connection `IP4.DNS` — is fully populated. The
/// known-good reference dispatcher for openconnect/GlobalProtect works around
/// the same race with a `sleep 1` before reading. `detect` runs on a blocking
/// thread (`spawn_blocking`), so it may block briefly: it reads, and on an
/// empty result waits `SETTLE_DELAY` then re-reads, up to `SETTLE_ATTEMPTS`
/// times. A non-empty result returns immediately. Total sleeping is bounded to
/// `(SETTLE_ATTEMPTS - 1) * SETTLE_DELAY` — here ~0.9 s, with the last read at
/// ~900 ms. That deliberately *tracks* the reference's empirical ~1 s rather
/// than undercutting it: the budget must be at least as long as the race is
/// known to take, because a missed window degrades to `Ok(empty)` whose only
/// recovery is a re-emitted ACTIVATED (see `dbus::handle_state`). The bound
/// still caps blocking so detect never waits indefinitely when DNS genuinely
/// never appears.
const SETTLE_ATTEMPTS: u32 = 4;
const SETTLE_DELAY_MS: u64 = 300;
const SETTLE_DELAY: Duration = Duration::from_millis(SETTLE_DELAY_MS);

// Guard the "bounded, total <= ~1 s" budget at compile time — an *upper* bound
// only (it keeps detect from blocking too long). The lower bound, "wait at
// least as long as the reference's ~1 s race," is a deliberate choice of
// SETTLE_ATTEMPTS / SETTLE_DELAY above, not enforced here. The settle test uses
// a no-op sleep counter, so it asserts the iteration bound but not the
// wall-clock one; bumping either constant past the 1 s budget is then a build
// error rather than a silent regression no test would catch.
const _: () = assert!(
    (SETTLE_ATTEMPTS as u64 - 1) * SETTLE_DELAY_MS <= 1000,
    "settle sleep budget must stay within ~1s (see SETTLE_ATTEMPTS / SETTLE_DELAY)"
);

/// The `nmcli` queries the DNS-discovery fallback chain needs, behind a trait so
/// the chain and its settle/retry can be unit-tested without a live
/// NetworkManager.
trait NmcliSource {
    /// `nmcli device show <iface>`. `Err` only on an *actual* failure — non-zero
    /// exit or the device truly absent; this is the up-ness gate. `Ok(stdout)`
    /// otherwise, including when the device carries no `IP4.DNS`.
    fn device_show(&self, iface: &str) -> Result<String, PlatformError>;

    /// `nmcli -t -f UUID,TYPE,STATE connection show --active`.
    fn active_connections(&self) -> Result<String, PlatformError>;

    /// `nmcli connection show <uuid>`.
    fn connection_show(&self, uuid: &str) -> Result<String, PlatformError>;

    /// Sleep one `SETTLE_DELAY` between fallback-chain reads.
    fn settle_sleep(&self);
}

/// Live `nmcli`-backed [`NmcliSource`].
struct RealNmcli;

/// An `nmcli` invocation with a forced C locale. We parse nmcli's output, and NM
/// localizes some field *values* under the host locale — notably the connection
/// `STATE` token `parse_active_vpn_uuids` matches on (`activated`). Without this
/// a non-English host fails to find the active VPN UUID, so openconnect/GP DNS
/// degrades to empty and the pushed DNS is never applied. The nmcli manual
/// recommends `LC_ALL=C` for exactly this machine-parsing case.
fn nmcli() -> Command {
    let mut cmd = Command::new("nmcli");
    cmd.env("LC_ALL", "C");
    cmd
}

impl NmcliSource for RealNmcli {
    fn device_show(&self, iface: &str) -> Result<String, PlatformError> {
        let output = nmcli().args(["device", "show", iface]).output()?;
        if !output.status.success() {
            return Err(PlatformError::VpnNotFound(iface.to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn active_connections(&self) -> Result<String, PlatformError> {
        let output = nmcli()
            .args([
                "-t",
                "-f",
                "UUID,TYPE,STATE",
                "connection",
                "show",
                "--active",
            ])
            .output()?;
        if !output.status.success() {
            return Err(PlatformError::CommandFailed(
                "nmcli connection show --active failed".to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn connection_show(&self, uuid: &str) -> Result<String, PlatformError> {
        let output = nmcli().args(["connection", "show", uuid]).output()?;
        if !output.status.success() {
            return Err(PlatformError::CommandFailed(format!(
                "nmcli connection show {uuid} failed"
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn settle_sleep(&self) {
        sleep(SETTLE_DELAY);
    }
}

/// DNS pushed onto the *single* active VPN connection, if one can be
/// unambiguously identified. Returns an empty `Vec` when there is no active VPN
/// connection, when the one found pushed no DNS, or when more than one is active
/// (see the multi-VPN note). Errors enumerating or reading connections are
/// non-fatal here — up-ness is already known from the device — so they degrade
/// to an empty result and are logged, never propagated.
///
/// Multi-VPN limitation: NM does not expose, via `nmcli`, which VPN active
/// connection owns a given tun device — the VPN active connection's `device` is
/// the *base* interface, not the tun. So this assumes exactly one active VPN
/// connection; with more than one we do not guess, we skip and log. This matches
/// Splitway's current one-VPN model.
fn vpn_connection_dns(source: &impl NmcliSource, iface: &str) -> Vec<String> {
    let active = match source.active_connections() {
        Ok(out) => out,
        Err(e) => {
            log::debug!("{iface}: cannot list active connections: {e}");
            return Vec::new();
        }
    };

    let uuids = parse_active_vpn_uuids(&active);
    match uuids.as_slice() {
        [] => Vec::new(),
        [uuid] => match source.connection_show(uuid) {
            Ok(out) => extract_dns_from_nmcli(&out),
            Err(e) => {
                log::debug!("{iface}: cannot read VPN connection {uuid}: {e}");
                Vec::new()
            }
        },
        many => {
            log::warn!(
                "{iface}: {} active VPN connections found; nmcli cannot attribute \
                 pushed DNS to a tun device, skipping connection-level DNS lookup",
                many.len()
            );
            Vec::new()
        }
    }
}

/// One pass of the DNS-source fallback chain. Returns the first non-empty DNS
/// set, or an empty `Vec` when none is found. Propagates only a genuine
/// device-show failure (device truly absent / `nmcli` non-zero exit), which
/// gates up-ness.
fn discover_dns_once(source: &impl NmcliSource, iface: &str) -> Result<Vec<String>, PlatformError> {
    // 1. DNS pushed onto the device itself (e.g. WireGuard). This call is also
    //    the up-ness gate: a non-zero exit / absent device is a real failure.
    let device_dns = extract_dns_from_nmcli(&source.device_show(iface)?);
    if !device_dns.is_empty() {
        return Ok(device_dns);
    }

    // 2. DNS pushed onto the single active VPN connection (openconnect/GP, which
    //    attaches its pushed DNS to the VPN connection, not the tun device).
    let conn_dns = vpn_connection_dns(source, iface);
    if !conn_dns.is_empty() {
        return Ok(conn_dns);
    }

    // 3. TODO(extension hook): systemd-resolved link read-back for `iface` as a
    //    plugin-agnostic last resort — read the link's resolvers straight from
    //    resolved when neither the device nor the VPN connection exposes them.
    //    Intentionally left unimplemented; tracked as a follow-up.

    Ok(Vec::new())
}

/// Run [`discover_dns_once`] with settle/retry: read, and on an empty result
/// wait `SETTLE_DELAY` and re-read, up to `SETTLE_ATTEMPTS` times, to bridge the
/// window where NM has signalled device-state `100` but not yet populated the
/// pushed DNS. A non-empty result returns immediately; a device-show failure
/// propagates immediately. Bounded: at most `SETTLE_ATTEMPTS` reads.
fn discover_dns(source: &impl NmcliSource, iface: &str) -> Result<Vec<String>, PlatformError> {
    for attempt in 1..=SETTLE_ATTEMPTS {
        let dns = discover_dns_once(source, iface)?;
        if !dns.is_empty() {
            return Ok(dns);
        }
        if attempt < SETTLE_ATTEMPTS {
            log::debug!(
                "{iface}: no pushed DNS yet (attempt {attempt}/{SETTLE_ATTEMPTS}), settling"
            );
            source.settle_sleep();
        }
    }
    Ok(Vec::new())
}

impl VpnDetector for LinuxDetector {
    fn detect(&self, interface: &str) -> Result<VpnInfo, PlatformError> {
        // Up-ness is decided by the device being present (the `?` below). DNS is
        // discovered separately and may legitimately be empty (a VPN that pushed
        // no DNS): that is an `Ok` with empty `dns_servers`, never an error, so
        // the watch path still emits `Up`. The state machine no-ops an `Up` with
        // empty DNS (see `StateMachine::desired`).
        let dns_servers = discover_dns(&RealNmcli, interface)?;

        Ok(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers,
        })
    }

    /// Spawns the NetworkManager D-Bus watch task on the ambient tokio
    /// runtime. Returns a `PlatformError` if called outside one; the
    /// `watch` subcommand handler owns the runtime until the daemon goes
    /// async in Phase 2.
    fn watch(
        &self,
        interface: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<VpnEvent>, PlatformError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|e| {
            PlatformError::CommandFailed(format!("watch requires a running tokio runtime: {e}"))
        })?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let interface = interface.to_string();
        handle.spawn(async move {
            log::debug!("starting NetworkManager watch for {interface}");
            if let Err(e) = super::dbus::watch_loop(interface.clone(), tx).await {
                log::error!("VPN watch for {interface} terminated: {e}");
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::{discover_dns, NmcliSource, SETTLE_ATTEMPTS};
    use splitway_shared::platform::PlatformError;
    use std::cell::Cell;

    // ---- Synthetic fixtures (RFC 5737 / RFC 3849 placeholders only) ----------

    /// WireGuard-style device: NM puts the pushed DNS on the device, so step 1
    /// of the chain wins and no connection lookup is needed.
    const DEVICE_WITH_DNS: &str = "\
GENERAL.DEVICE:                         wg0
GENERAL.TYPE:                           wireguard
GENERAL.STATE:                          100 (connected)
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.DNS[1]:                             192.0.2.1
";

    /// openconnect/GlobalProtect device: routes but no device-level DNS.
    const DEVICE_NO_DNS: &str = "\
GENERAL.DEVICE:                         tun0
GENERAL.TYPE:                           tun
GENERAL.STATE:                          100 (connected (externally))
IP4.ADDRESS[1]:                         192.0.2.2/32
IP4.ROUTE[1]:                           dst = 198.51.100.0/24, nh = 0.0.0.0, mt = 50
IP6.GATEWAY:                            --
";

    /// `nmcli connection show <uuid>` for the active VPN connection, carrying
    /// the pushed DNS on its runtime `IP4.DNS[n]` lines.
    const CONN_WITH_DNS: &str = "\
connection.type:                        vpn
vpn.service-type:                       org.freedesktop.NetworkManager.openconnect
ipv4.dns:                               --
VPN.TYPE:                               openconnect
IP4.DNS[1]:                             203.0.113.53
IP6.DNS[1]:                             2001:db8::53
";

    /// Same connection, before NM has populated the pushed DNS (the settle race).
    const CONN_NO_DNS: &str = "\
connection.type:                        vpn
vpn.service-type:                       org.freedesktop.NetworkManager.openconnect
ipv4.dns:                               --
VPN.TYPE:                               openconnect
";

    const ACTIVE_ONE_VPN: &str = "\
11111111-1111-1111-1111-111111111111:vpn:activated
22222222-2222-2222-2222-222222222222:ethernet:activated
";

    const ACTIVE_NO_VPN: &str = "\
22222222-2222-2222-2222-222222222222:ethernet:activated
";

    const ACTIVE_TWO_VPN: &str = "\
11111111-1111-1111-1111-111111111111:vpn:activated
33333333-3333-3333-3333-333333333333:vpn:activated
";

    /// Scripted [`NmcliSource`]. `connection_show` walks `conn_outputs`, clamping
    /// at the last entry, so a settle test can yield empty-then-populated DNS
    /// across retries. Call counters let tests assert which steps ran.
    struct FakeNmcli {
        device: Result<String, ()>, // Err => device absent (up-ness gate fails)
        active: Result<String, ()>, // Err => `connection show --active` failed
        conn_outputs: Vec<String>,
        conn_fails: bool, // `connection show <uuid>` fails
        device_calls: Cell<usize>,
        active_calls: Cell<usize>,
        conn_calls: Cell<usize>,
        sleeps: Cell<usize>,
    }

    impl FakeNmcli {
        fn new(device: &str, active: &str, conn_outputs: &[&str]) -> Self {
            Self {
                device: Ok(device.to_string()),
                active: Ok(active.to_string()),
                conn_outputs: conn_outputs.iter().map(|s| s.to_string()).collect(),
                conn_fails: false,
                device_calls: Cell::new(0),
                active_calls: Cell::new(0),
                conn_calls: Cell::new(0),
                sleeps: Cell::new(0),
            }
        }

        fn absent() -> Self {
            let mut me = Self::new("", "", &[]);
            me.device = Err(());
            me
        }

        /// `nmcli connection show --active` returns non-zero / errors.
        fn with_active_error(mut self) -> Self {
            self.active = Err(());
            self
        }

        /// `nmcli connection show <uuid>` returns non-zero / errors.
        fn with_conn_error(mut self) -> Self {
            self.conn_fails = true;
            self
        }
    }

    impl NmcliSource for FakeNmcli {
        fn device_show(&self, iface: &str) -> Result<String, PlatformError> {
            self.device_calls.set(self.device_calls.get() + 1);
            self.device
                .clone()
                .map_err(|()| PlatformError::VpnNotFound(iface.to_string()))
        }

        fn active_connections(&self) -> Result<String, PlatformError> {
            self.active_calls.set(self.active_calls.get() + 1);
            self.active
                .clone()
                .map_err(|()| PlatformError::CommandFailed("active list failed".to_string()))
        }

        fn connection_show(&self, uuid: &str) -> Result<String, PlatformError> {
            if self.conn_fails {
                self.conn_calls.set(self.conn_calls.get() + 1);
                return Err(PlatformError::CommandFailed(format!("conn {uuid} failed")));
            }
            let n = self.conn_calls.get();
            self.conn_calls.set(n + 1);
            let idx = n.min(self.conn_outputs.len().saturating_sub(1));
            Ok(self.conn_outputs.get(idx).cloned().unwrap_or_default())
        }

        fn settle_sleep(&self) {
            self.sleeps.set(self.sleeps.get() + 1);
        }
    }

    #[test]
    fn device_dns_wins_without_connection_lookup() {
        let fake = FakeNmcli::new(DEVICE_WITH_DNS, ACTIVE_ONE_VPN, &[CONN_WITH_DNS]);
        let dns = discover_dns(&fake, "wg0").unwrap();

        assert_eq!(dns, vec!["192.0.2.1".to_string()]);
        // Step 1 short-circuits: no fallback, no settle.
        assert_eq!(fake.active_calls.get(), 0);
        assert_eq!(fake.conn_calls.get(), 0);
        assert_eq!(fake.sleeps.get(), 0);
    }

    #[test]
    fn falls_back_to_single_active_vpn_connection() {
        let fake = FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_ONE_VPN, &[CONN_WITH_DNS]);
        let dns = discover_dns(&fake, "tun0").unwrap();

        assert_eq!(
            dns,
            vec!["203.0.113.53".to_string(), "2001:db8::53".to_string()]
        );
        assert_eq!(fake.conn_calls.get(), 1);
        assert_eq!(fake.sleeps.get(), 0);
    }

    #[test]
    fn no_device_dns_and_no_vpn_dns_returns_empty_ok() {
        let fake = FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_NO_VPN, &[CONN_WITH_DNS]);
        let dns = discover_dns(&fake, "tun0").unwrap();

        assert!(dns.is_empty());
        // No active VPN connection to read.
        assert_eq!(fake.conn_calls.get(), 0);
        // Settled the full bounded number of times before giving up.
        assert_eq!(fake.sleeps.get() as u32, SETTLE_ATTEMPTS - 1);
    }

    #[test]
    fn settle_retries_until_connection_dns_appears() {
        // Empty on the first read, populated on the second (the NM settle race).
        let fake = FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_ONE_VPN, &[CONN_NO_DNS, CONN_WITH_DNS]);
        let dns = discover_dns(&fake, "tun0").unwrap();

        assert_eq!(
            dns,
            vec!["203.0.113.53".to_string(), "2001:db8::53".to_string()]
        );
        assert_eq!(fake.conn_calls.get(), 2);
        assert_eq!(fake.sleeps.get(), 1);
    }

    #[test]
    fn settle_is_bounded_when_dns_never_appears() {
        let fake = FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_ONE_VPN, &[CONN_NO_DNS]);
        let dns = discover_dns(&fake, "tun0").unwrap();

        assert!(dns.is_empty());
        // Read exactly SETTLE_ATTEMPTS times, slept exactly SETTLE_ATTEMPTS - 1.
        assert_eq!(fake.conn_calls.get() as u32, SETTLE_ATTEMPTS);
        assert_eq!(fake.sleeps.get() as u32, SETTLE_ATTEMPTS - 1);
    }

    #[test]
    fn multiple_active_vpns_skip_connection_lookup() {
        let fake = FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_TWO_VPN, &[CONN_WITH_DNS]);
        let dns = discover_dns(&fake, "tun0").unwrap();

        assert!(dns.is_empty());
        // Ambiguous attribution: never read any connection.
        assert_eq!(fake.conn_calls.get(), 0);
    }

    #[test]
    fn absent_device_is_error_not_empty() {
        let fake = FakeNmcli::absent();
        let err = discover_dns(&fake, "tun0").unwrap_err();

        assert!(matches!(err, PlatformError::VpnNotFound(_)));
        // Failed up-ness gate: never tried the fallback chain.
        assert_eq!(fake.conn_calls.get(), 0);
        assert_eq!(fake.sleeps.get(), 0);
    }

    // A step-2 failure is non-fatal: up-ness is already known from the device,
    // so a failed connection lookup degrades to empty DNS (Ok, not Err) and
    // never suppresses Up.

    #[test]
    fn connection_read_failure_degrades_to_empty_ok() {
        let fake =
            FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_ONE_VPN, &[CONN_WITH_DNS]).with_conn_error();
        let dns = discover_dns(&fake, "tun0").unwrap(); // Ok, never Err

        assert!(dns.is_empty());
        // Tried the connection on every settle attempt; never propagated.
        assert_eq!(fake.conn_calls.get() as u32, SETTLE_ATTEMPTS);
    }

    #[test]
    fn active_connection_listing_failure_degrades_to_empty_ok() {
        let fake =
            FakeNmcli::new(DEVICE_NO_DNS, ACTIVE_ONE_VPN, &[CONN_WITH_DNS]).with_active_error();
        let dns = discover_dns(&fake, "tun0").unwrap(); // Ok, never Err

        assert!(dns.is_empty());
        // Listing failed before any single connection could be read.
        assert_eq!(fake.conn_calls.get(), 0);
    }
}
