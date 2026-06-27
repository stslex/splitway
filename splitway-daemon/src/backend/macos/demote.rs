//! Demoting a hijacked system **default** resolver so non-corp DNS resolves
//! off-tunnel — the second half of macOS DNS privacy (the first half being the
//! per-domain `/etc/resolver` scope in [`super::backend`]).
//!
//! # Why a demote is needed
//!
//! When the VPN client registers the corp resolver as the system *default*
//! (rather than scoping it to the tunnel), every query that is **not** for a
//! corp domain would otherwise go to the corp resolver, over the tunnel —
//! exactly the privacy leak Splitway exists to prevent. Demoting overwrites the
//! primary network service's `ServerAddresses` (the source of the global
//! default) with an off-tunnel **fallback** resolver, so only the corp domains
//! (pinned by the `/etc/resolver` files) reach the corp resolver.
//!
//! # Reversibility (the load-bearing property)
//!
//! The demote must never leave the machine with a broken or half-changed
//! resolver. Before overwriting, the prior `ServerAddresses` is **snapshotted to
//! disk** ([`SnapshotStore`]); restore rewrites exactly that. The snapshot lives
//! on disk (not just in memory) so an unclean exit — the daemon SIGKILLed
//! between demote and a later revert — can still be undone on the next start.
//!
//! # Testability
//!
//! All `scutil` contact goes through the [`ScutilRunner`] seam: the real impl
//! shells out, tests inject a fake that *captures the exact script issued* and
//! returns canned state, so the demote/restore logic is unit-tested without
//! touching the live system. The script text itself is built by pure functions
//! ([`build_set_dns_script`] / `build_*`) that are tested directly.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use splitway_shared::platform::PlatformError;

/// The dynamic-store key holding the primary network service id we demote.
const GLOBAL_IPV4_KEY: &str = "State:/Network/Global/IPv4";

/// The snapshot file: the pre-demote primary-service DNS, captured so revert can
/// restore it byte-for-byte even across an unclean daemon exit. Lives under the
/// daemon's runtime dir (root-owned, like the socket).
pub(super) const SNAPSHOT_PATH: &str = "/var/run/splitway/dns-demote.snapshot";

/// A captured pre-demote state: which service was demoted, the `InterfaceName`
/// it was bound to, and the `ServerAddresses` it had before.
///
/// Serialised to [`SNAPSHOT_PATH`] as a tiny self-describing line format (no
/// serde dependency pulled in for one struct): each line is `<tag>\t<value>`,
/// with tags `key` (the service DNS key, exactly once), `iface` (the
/// `InterfaceName`, at most once — omitted when the service had none), and
/// `server` (one per prior resolver, in order). The tags make the optional
/// `iface` line unambiguous even when there are no servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DemoteSnapshot {
    /// The full `State:/Network/Service/<id>/DNS` key that was overwritten.
    pub service_dns_key: String,
    /// The `InterfaceName` the service was bound to before the demote, if any.
    /// Re-applied on demote so the demoted service stays identifiable as the
    /// physical service by the detector (a `d.init` write would otherwise drop
    /// it — see [`build_set_dns_script`] and the detector's `decide`).
    pub interface_name: Option<String>,
    /// The `ServerAddresses` the service had before the demote (possibly empty
    /// if the service carried no explicit servers — restore then clears ours).
    pub prior_servers: Vec<String>,
}

impl DemoteSnapshot {
    /// Serialise to the on-disk `<tag>\t<value>` line format.
    fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str("key\t");
        out.push_str(&self.service_dns_key);
        out.push('\n');
        if let Some(iface) = &self.interface_name {
            out.push_str("iface\t");
            out.push_str(iface);
            out.push('\n');
        }
        for s in &self.prior_servers {
            out.push_str("server\t");
            out.push_str(s);
            out.push('\n');
        }
        out
    }

    /// Parse the on-disk `<tag>\t<value>` line format. Requires a `key` line;
    /// `iface` is optional; `server` lines are collected in order. Returns
    /// `None` for an empty/garbled snapshot (no `key`).
    fn deserialize(text: &str) -> Option<Self> {
        let mut service_dns_key: Option<String> = None;
        let mut interface_name: Option<String> = None;
        let mut prior_servers: Vec<String> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches(['\r', '\n']);
            let Some((tag, value)) = line.split_once('\t') else {
                continue; // skip a malformed line rather than failing the whole load
            };
            match tag {
                "key" => service_dns_key = Some(value.to_string()),
                "iface" if !value.is_empty() => interface_name = Some(value.to_string()),
                "server" if !value.is_empty() => prior_servers.push(value.to_string()),
                _ => {}
            }
        }
        Some(DemoteSnapshot {
            service_dns_key: service_dns_key?,
            interface_name,
            prior_servers,
        })
    }
}

/// The current DNS dict of one service, as read from
/// `State:/Network/Service/<id>/DNS` — the bits the demote must snapshot and
/// preserve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ServiceDnsState {
    /// The `InterfaceName` the service is bound to, if present.
    pub interface_name: Option<String>,
    /// The service's `ServerAddresses`.
    pub servers: Vec<String>,
}

/// The seam over `scutil`: read the bits the demote needs, and run a script.
/// The real impl ([`RealScutil`]) shells out; tests inject a fake.
pub(super) trait ScutilRunner {
    /// The primary network service id from `State:/Network/Global/IPv4`'s
    /// `PrimaryService`, or `None` if there is no primary service (offline).
    fn primary_service(&self) -> Result<Option<String>, PlatformError>;

    /// The current DNS dict (servers + `InterfaceName`) of
    /// `State:/Network/Service/<id>/DNS`.
    fn service_dns_state(&self, service_dns_key: &str) -> Result<ServiceDnsState, PlatformError>;

    /// Run a `scutil` script (the text piped to `scutil`'s stdin).
    fn run_script(&self, script: &str) -> Result<(), PlatformError>;
}

/// Build the `scutil` script that sets `key`'s `ServerAddresses` to `servers`,
/// preserving the service's `InterfaceName` when known.
///
/// We replace only the values we manage; SearchDomains and the rest of the dict
/// are not ours to keep — macOS repopulates a service's DNS dict from its source
/// (DHCP / the VPN) on the next network change, and the corp split-DNS is held
/// independently by the `/etc/resolver` files, so a minimal dict is sufficient.
///
/// **`InterfaceName` is re-added when known** because a bare `d.init` write would
/// drop it, and the detector identifies the *physical* service by its
/// `InterfaceName == PrimaryInterface` (see the detector's `decide`). Dropping it
/// on our own demote would make the next detection round fail to find the
/// physical service — inverting corp/fallback or undoing the demote (the exact
/// oscillation the per-service model exists to prevent). So the demote carries
/// the interface name through.
pub(super) fn build_set_dns_script(
    key: &str,
    servers: &[String],
    interface_name: Option<&str>,
) -> String {
    let mut script = String::new();
    script.push_str("open\n");
    script.push_str("d.init\n");
    if !servers.is_empty() {
        script.push_str("d.add ServerAddresses *");
        for s in servers {
            script.push(' ');
            script.push_str(s);
        }
        script.push('\n');
    }
    if let Some(iface) = interface_name {
        // A single-value key (no `*`): the interface the service is bound to.
        script.push_str(&format!("d.add InterfaceName {iface}\n"));
    }
    script.push_str(&format!("set {key}\n"));
    script.push_str("quit\n");
    script
}

/// Build the `scutil` script that removes `key` entirely (used on restore when
/// the service had no prior explicit DNS — clearing ours lets SC repopulate the
/// service's DNS from its real source).
pub(super) fn build_remove_key_script(key: &str) -> String {
    format!("open\nremove {key}\nquit\n")
}

/// The `State:/Network/Service/<id>/DNS` key for a service id.
pub(super) fn service_dns_key(service_id: &str) -> String {
    format!("State:/Network/Service/{service_id}/DNS")
}

/// The real `scutil`-backed [`ScutilRunner`].
pub(super) struct RealScutil;

impl ScutilRunner for RealScutil {
    fn primary_service(&self) -> Result<Option<String>, PlatformError> {
        let dump = run_scutil(&format!("show {GLOBAL_IPV4_KEY}\n"))?;
        Ok(crate::detector::macos_parse_scalar_field(
            &dump,
            "PrimaryService",
        ))
    }

    fn service_dns_state(&self, service_dns_key: &str) -> Result<ServiceDnsState, PlatformError> {
        let dump = run_scutil(&format!("show {service_dns_key}\n"))?;
        Ok(ServiceDnsState {
            interface_name: crate::detector::macos_parse_scalar_field(&dump, "InterfaceName"),
            servers: crate::detector::macos_parse_array_field(&dump, "ServerAddresses"),
        })
    }

    fn run_script(&self, script: &str) -> Result<(), PlatformError> {
        run_scutil(script).map(|_| ())
    }
}

/// Pipe `script` to `scutil`'s stdin and return its stdout.
fn run_scutil(script: &str) -> Result<String, PlatformError> {
    let mut child = Command::new("scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PlatformError::CommandFailed(format!("failed to spawn scutil: {e}")))?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            PlatformError::CommandFailed("scutil stdin was not captured".to_string())
        })?;
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

/// Reads/writes the on-disk demote snapshot. A trait so tests inject a temp-dir
/// store; the real impl uses [`SNAPSHOT_PATH`].
pub(super) trait SnapshotStore {
    fn load(&self) -> Option<DemoteSnapshot>;
    fn save(&self, snapshot: &DemoteSnapshot) -> Result<(), PlatformError>;
    fn clear(&self);
}

/// The real on-disk snapshot store at [`SNAPSHOT_PATH`].
pub(super) struct FileSnapshotStore {
    path: PathBuf,
}

impl FileSnapshotStore {
    pub(super) fn new() -> Self {
        FileSnapshotStore {
            path: PathBuf::from(SNAPSHOT_PATH),
        }
    }
}

impl SnapshotStore for FileSnapshotStore {
    fn load(&self) -> Option<DemoteSnapshot> {
        let text = fs::read_to_string(&self.path).ok()?;
        DemoteSnapshot::deserialize(&text)
    }

    fn save(&self, snapshot: &DemoteSnapshot) -> Result<(), PlatformError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                PlatformError::CommandFailed(format!(
                    "creating snapshot dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        // atomic_write keeps the snapshot intact on a crash mid-write.
        splitway_shared::config::atomic_write(&self.path, snapshot.serialize().as_bytes())
            .map_err(|e| PlatformError::CommandFailed(format!("writing demote snapshot: {e}")))
    }

    fn clear(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Demote the system default resolver to `fallback`, snapshotting the prior
/// primary-service DNS first so [`restore`] can undo it. Idempotent: re-running
/// with the same fallback is a no-op-equivalent (it re-sets the same servers);
/// the snapshot is only captured on the *first* demote (when none exists), so a
/// re-demote never overwrites the original prior state with our own fallback.
///
/// Returns `Ok(false)` (not an error) when there is no primary service to demote
/// — the caller treats that as "nothing to do".
pub(super) fn demote(
    scutil: &dyn ScutilRunner,
    snapshots: &dyn SnapshotStore,
    fallback: &[String],
) -> Result<bool, PlatformError> {
    if fallback.is_empty() {
        return Err(PlatformError::CommandFailed(
            "refusing to demote the system default to an empty resolver set".to_string(),
        ));
    }
    let Some(service_id) = scutil.primary_service()? else {
        log::warn!("no primary network service; skipping default-DNS demote");
        return Ok(false);
    };
    let key = service_dns_key(&service_id);

    // Snapshot handling, keyed by the service we are about to demote:
    //
    // - No snapshot yet → capture this service's prior DNS (the first demote).
    // - A snapshot for THIS SAME service → keep it (a reconcile re-apply must not
    //   snapshot our own fallback as the "prior").
    // - A snapshot for a DIFFERENT service (the primary changed while up — e.g. a
    //   Wi-Fi↔Ethernet switch) → restore that previous service from its snapshot
    //   first, so it is not left stranded on our fallback, then capture and demote
    //   the new primary. Exactly one service is ever demoted at a time.
    match snapshots.load() {
        Some(existing) if existing.service_dns_key == key => {
            // Same service — keep the original prior snapshot.
        }
        Some(existing) => {
            // Primary changed: un-demote the old service, then snapshot the new.
            restore_snapshot(scutil, &existing)?;
            snapshots.clear();
            capture_snapshot(scutil, snapshots, &key)?;
        }
        None => {
            capture_snapshot(scutil, snapshots, &key)?;
        }
    }

    // Re-apply the interface name we captured so the demoted (physical) service
    // stays identifiable to the detector (see build_set_dns_script).
    let interface_name = snapshots.load().and_then(|s| s.interface_name);
    scutil.run_script(&build_set_dns_script(
        &key,
        fallback,
        interface_name.as_deref(),
    ))?;
    log::info!("demoted the system default resolver to the off-tunnel fallback");
    Ok(true)
}

/// Capture `key`'s current DNS dict (servers + interface name) into the
/// snapshot store, before it is overwritten by a demote.
fn capture_snapshot(
    scutil: &dyn ScutilRunner,
    snapshots: &dyn SnapshotStore,
    key: &str,
) -> Result<(), PlatformError> {
    let state = scutil.service_dns_state(key)?;
    snapshots.save(&DemoteSnapshot {
        service_dns_key: key.to_string(),
        interface_name: state.interface_name,
        prior_servers: state.servers,
    })
}

/// Restore the demoted default from the snapshot, then clear it. A missing
/// snapshot is a clean no-op (nothing was demoted, or already restored). Always
/// clears the snapshot on success so a later demote re-snaps.
pub(super) fn restore(
    scutil: &dyn ScutilRunner,
    snapshots: &dyn SnapshotStore,
) -> Result<(), PlatformError> {
    let Some(snapshot) = snapshots.load() else {
        return Ok(()); // nothing demoted
    };
    restore_snapshot(scutil, &snapshot)?;
    snapshots.clear();
    log::info!("restored the system default resolver from the demote snapshot");
    Ok(())
}

/// Run the `scutil` write that restores one snapshot's service to its prior DNS.
/// If the service had explicit prior servers, set them back (preserving the
/// captured interface name); if it had none, remove the key so
/// SystemConfiguration repopulates the service's DNS from its real source. Does
/// not touch the snapshot store — the caller owns clearing it.
fn restore_snapshot(
    scutil: &dyn ScutilRunner,
    snapshot: &DemoteSnapshot,
) -> Result<(), PlatformError> {
    let script = if snapshot.prior_servers.is_empty() {
        build_remove_key_script(&snapshot.service_dns_key)
    } else {
        build_set_dns_script(
            &snapshot.service_dns_key,
            &snapshot.prior_servers,
            snapshot.interface_name.as_deref(),
        )
    };
    scutil.run_script(&script)
}

/// Test doubles for the demote seam, shared by this module's tests and the
/// backend's apply/revert wiring tests.
#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use std::cell::RefCell;

    /// A fake `scutil` that captures every script run and returns canned state.
    /// `primary` is interior-mutable so a test can model the primary service
    /// changing while up (the P2 case). `service_states` maps a service DNS key
    /// to the dict the fake reports for it; an unmapped key falls back to
    /// `default_state`.
    pub(in super::super) struct FakeScutil {
        pub primary: RefCell<Option<String>>,
        pub default_state: ServiceDnsState,
        pub service_states: RefCell<std::collections::HashMap<String, ServiceDnsState>>,
        pub scripts: RefCell<Vec<String>>,
        pub fail_on_set: std::cell::Cell<bool>,
    }

    impl FakeScutil {
        /// A primary service ("ABC", on en0) with one prior resolver — the
        /// common case.
        pub fn up() -> Self {
            FakeScutil {
                primary: RefCell::new(Some("ABC".to_string())),
                default_state: ServiceDnsState {
                    interface_name: Some("en0".to_string()),
                    servers: vec!["198.51.100.1".to_string()],
                },
                service_states: RefCell::new(std::collections::HashMap::new()),
                scripts: RefCell::new(Vec::new()),
                fail_on_set: std::cell::Cell::new(false),
            }
        }

        /// Make subsequent `set` scripts fail (to test rollback paths).
        pub fn fail_on_set(&self) {
            self.fail_on_set.set(true);
        }

        /// Point `primary_service()` at a different service id (P2: the primary
        /// changed while up).
        pub fn set_primary(&self, id: &str) {
            *self.primary.borrow_mut() = Some(id.to_string());
        }

        /// Script the DNS dict the fake reports for a specific service key.
        pub fn set_service_state(&self, key: &str, state: ServiceDnsState) {
            self.service_states
                .borrow_mut()
                .insert(key.to_string(), state);
        }
    }

    impl ScutilRunner for FakeScutil {
        fn primary_service(&self) -> Result<Option<String>, PlatformError> {
            Ok(self.primary.borrow().clone())
        }
        fn service_dns_state(&self, key: &str) -> Result<ServiceDnsState, PlatformError> {
            Ok(self
                .service_states
                .borrow()
                .get(key)
                .cloned()
                .unwrap_or_else(|| self.default_state.clone()))
        }
        fn run_script(&self, script: &str) -> Result<(), PlatformError> {
            if self.fail_on_set.get() && script.contains("set ") {
                return Err(PlatformError::CommandFailed("simulated set failure".into()));
            }
            self.scripts.borrow_mut().push(script.to_string());
            Ok(())
        }
    }

    /// An in-memory snapshot store for tests.
    #[derive(Default)]
    pub(in super::super) struct MemSnapshots {
        pub slot: RefCell<Option<DemoteSnapshot>>,
    }
    impl SnapshotStore for MemSnapshots {
        fn load(&self) -> Option<DemoteSnapshot> {
            self.slot.borrow().clone()
        }
        fn save(&self, snapshot: &DemoteSnapshot) -> Result<(), PlatformError> {
            *self.slot.borrow_mut() = Some(snapshot.clone());
            Ok(())
        }
        fn clear(&self) {
            *self.slot.borrow_mut() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeScutil, MemSnapshots};
    use super::*;

    fn snap(key: &str, iface: Option<&str>, servers: &[&str]) -> DemoteSnapshot {
        DemoteSnapshot {
            service_dns_key: key.to_string(),
            interface_name: iface.map(str::to_string),
            prior_servers: servers.iter().map(|s| s.to_string()).collect(),
        }
    }

    // --- pure script builders -------------------------------------------------

    #[test]
    fn set_dns_script_sets_server_addresses_and_preserves_the_interface() {
        let script = build_set_dns_script(
            "State:/Network/Service/ABC/DNS",
            &["198.51.100.1".to_string()],
            Some("en0"),
        );
        assert_eq!(
            script,
            "open\n\
             d.init\n\
             d.add ServerAddresses * 198.51.100.1\n\
             d.add InterfaceName en0\n\
             set State:/Network/Service/ABC/DNS\n\
             quit\n"
        );
    }

    #[test]
    fn set_dns_script_omits_the_interface_when_unknown() {
        let script = build_set_dns_script(
            "State:/Network/Service/ABC/DNS",
            &["198.51.100.1".to_string()],
            None,
        );
        assert!(!script.contains("InterfaceName"));
        assert!(script.contains("d.add ServerAddresses * 198.51.100.1\n"));
    }

    #[test]
    fn set_dns_script_handles_multiple_servers() {
        let script = build_set_dns_script(
            "State:/Network/Service/ABC/DNS",
            &["198.51.100.1".to_string(), "198.51.100.2".to_string()],
            Some("en0"),
        );
        assert!(script.contains("d.add ServerAddresses * 198.51.100.1 198.51.100.2\n"));
    }

    #[test]
    fn set_dns_script_with_no_servers_omits_the_add_but_keeps_the_interface() {
        let script = build_set_dns_script("State:/Network/Service/ABC/DNS", &[], Some("en0"));
        assert!(!script.contains("d.add ServerAddresses"));
        assert!(script.contains("d.add InterfaceName en0\n"));
        assert!(script.contains("set State:/Network/Service/ABC/DNS\n"));
    }

    #[test]
    fn remove_key_script_removes_the_key() {
        assert_eq!(
            build_remove_key_script("State:/Network/Service/ABC/DNS"),
            "open\nremove State:/Network/Service/ABC/DNS\nquit\n"
        );
    }

    #[test]
    fn service_dns_key_is_well_formed() {
        assert_eq!(
            service_dns_key("ABC-123"),
            "State:/Network/Service/ABC-123/DNS"
        );
    }

    #[test]
    fn snapshot_round_trips_through_disk_format() {
        let s = snap(
            "State:/Network/Service/ABC/DNS",
            Some("en0"),
            &["198.51.100.1", "198.51.100.2"],
        );
        assert_eq!(DemoteSnapshot::deserialize(&s.serialize()).unwrap(), s);
    }

    #[test]
    fn snapshot_round_trips_with_no_prior_servers_and_no_interface() {
        let s = snap("State:/Network/Service/ABC/DNS", None, &[]);
        assert_eq!(DemoteSnapshot::deserialize(&s.serialize()).unwrap(), s);
    }

    #[test]
    fn snapshot_round_trips_with_an_interface_but_no_servers() {
        let s = snap("State:/Network/Service/ABC/DNS", Some("en0"), &[]);
        assert_eq!(DemoteSnapshot::deserialize(&s.serialize()).unwrap(), s);
    }

    #[test]
    fn snapshot_deserialize_rejects_garbage_without_a_key() {
        assert!(DemoteSnapshot::deserialize("").is_none());
        assert!(DemoteSnapshot::deserialize("server\t1.2.3.4\n").is_none());
    }

    // --- demote / restore orchestration --------------------------------------

    #[test]
    fn demote_snapshots_prior_then_sets_the_fallback_preserving_the_interface() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        let did = demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        assert!(did);
        // Prior state captured for the primary service, including its interface.
        assert_eq!(
            snaps.load().unwrap(),
            snap(
                "State:/Network/Service/ABC/DNS",
                Some("en0"),
                &["198.51.100.1"],
            )
        );
        // Exactly one script issued: set the fallback AND re-add InterfaceName so
        // the demoted physical service stays identifiable (P1 fix).
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("d.add ServerAddresses * 203.0.113.9\n"));
        assert!(
            scripts[0].contains("d.add InterfaceName en0\n"),
            "the demote must preserve InterfaceName so detection still finds the physical service"
        );
        assert!(scripts[0].contains("set State:/Network/Service/ABC/DNS\n"));
    }

    #[test]
    fn redemote_does_not_overwrite_the_original_snapshot() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        let after_first = snaps.load().unwrap();
        // A second demote (same primary) must keep the ORIGINAL prior, not
        // snapshot our own fallback.
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        assert_eq!(snaps.load().unwrap(), after_first);
        assert_eq!(
            after_first.prior_servers,
            vec!["198.51.100.1".to_string()],
            "the snapshot must remain the real prior resolver, never our fallback"
        );
    }

    #[test]
    fn demote_on_a_changed_primary_restores_the_old_then_snapshots_the_new() {
        // P2: the primary service changes while up (e.g. Wi-Fi → Ethernet) with a
        // snapshot already present. The old service must be un-demoted (not left
        // stranded on our fallback), and the NEW service snapshotted + demoted.
        let scutil = FakeScutil::up(); // primary = ABC (en0)
        let snaps = MemSnapshots::default();
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        scutil.scripts.borrow_mut().clear();

        // The primary switches to XYZ on en1 with its own DHCP resolver.
        scutil.set_primary("XYZ");
        scutil.set_service_state(
            "State:/Network/Service/XYZ/DNS",
            ServiceDnsState {
                interface_name: Some("en1".to_string()),
                servers: vec!["198.51.100.50".to_string()],
            },
        );
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();

        // The snapshot now tracks the NEW primary with its real prior DNS.
        assert_eq!(
            snaps.load().unwrap(),
            snap(
                "State:/Network/Service/XYZ/DNS",
                Some("en1"),
                &["198.51.100.50"],
            )
        );
        let scripts = scutil.scripts.borrow();
        // First the OLD service is restored to its real prior (198.51.100.1),
        // then the NEW service is demoted to the fallback.
        assert!(
            scripts
                .iter()
                .any(|s| s.contains("set State:/Network/Service/ABC/DNS")
                    && s.contains("d.add ServerAddresses * 198.51.100.1")),
            "the previous primary must be restored to its real DNS, not left on the fallback"
        );
        assert!(
            scripts
                .iter()
                .any(|s| s.contains("set State:/Network/Service/XYZ/DNS")
                    && s.contains("d.add ServerAddresses * 203.0.113.9")),
            "the new primary must be demoted to the fallback"
        );
    }

    #[test]
    fn demote_with_no_primary_service_is_a_clean_noop() {
        let scutil = FakeScutil::up();
        *scutil.primary.borrow_mut() = None;
        let snaps = MemSnapshots::default();
        let did = demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        assert!(!did);
        assert!(snaps.load().is_none());
        assert!(scutil.scripts.borrow().is_empty());
    }

    #[test]
    fn demote_to_empty_fallback_is_rejected() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        assert!(demote(&scutil, &snaps, &[]).is_err());
        // Nothing captured or issued on rejection.
        assert!(snaps.load().is_none());
        assert!(scutil.scripts.borrow().is_empty());
    }

    #[test]
    fn restore_sets_prior_servers_with_the_interface_and_clears_snapshot() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        scutil.scripts.borrow_mut().clear();

        restore(&scutil, &snaps).unwrap();
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        // Restored the original servers AND the interface name...
        assert!(scripts[0].contains("d.add ServerAddresses * 198.51.100.1\n"));
        assert!(scripts[0].contains("d.add InterfaceName en0\n"));
        // ...and cleared the snapshot so a later demote re-snaps.
        drop(scripts);
        assert!(snaps.load().is_none());
    }

    #[test]
    fn restore_removes_the_key_when_no_prior_servers() {
        // A service with no explicit prior DNS: restore removes our override so
        // SC repopulates from the real source, rather than pinning empty.
        let scutil = FakeScutil::up();
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec![],
            },
        );
        let snaps = MemSnapshots::default();
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        scutil.scripts.borrow_mut().clear();

        restore(&scutil, &snaps).unwrap();
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].starts_with("open\nremove State:/Network/Service/ABC/DNS\n"));
    }

    #[test]
    fn restore_with_no_snapshot_is_a_clean_noop() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        restore(&scutil, &snaps).unwrap();
        assert!(scutil.scripts.borrow().is_empty());
    }

    #[test]
    fn demote_propagates_a_set_failure_after_snapshotting() {
        // If the set fails, the snapshot is already on disk, so a later restore
        // can still recover — the demote surfaces the error rather than masking.
        let scutil = FakeScutil::up();
        scutil.fail_on_set();
        let snaps = MemSnapshots::default();
        let result = demote(&scutil, &snaps, &["203.0.113.9".to_string()]);
        assert!(result.is_err());
        // Snapshot persisted so the half-done demote is recoverable on revert.
        assert!(snaps.load().is_some());
    }
}
