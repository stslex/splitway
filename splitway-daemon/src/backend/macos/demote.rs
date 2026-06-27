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

/// A captured pre-demote state: which service was demoted and the
/// `ServerAddresses` it had before. Serialised to [`SNAPSHOT_PATH`] as a tiny
/// line-based format (no serde dependency pulled into the backend for one
/// struct): first line = service key, remaining lines = one server each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DemoteSnapshot {
    /// The full `State:/Network/Service/<id>/DNS` key that was overwritten.
    pub service_dns_key: String,
    /// The `ServerAddresses` the service had before the demote (possibly empty
    /// if the service carried no explicit servers — restore then clears ours).
    pub prior_servers: Vec<String>,
}

impl DemoteSnapshot {
    /// Serialise to the on-disk line format.
    fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.service_dns_key);
        out.push('\n');
        for s in &self.prior_servers {
            out.push_str(s);
            out.push('\n');
        }
        out
    }

    /// Parse the on-disk line format. The first non-empty line is the key; the
    /// rest are servers. Returns `None` for an empty/garbled snapshot.
    fn deserialize(text: &str) -> Option<Self> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let service_dns_key = lines.next()?.to_string();
        let prior_servers = lines.map(str::to_string).collect();
        Some(DemoteSnapshot {
            service_dns_key,
            prior_servers,
        })
    }
}

/// The seam over `scutil`: read the bits the demote needs, and run a script.
/// The real impl ([`RealScutil`]) shells out; tests inject a fake.
pub(super) trait ScutilRunner {
    /// The primary network service id from `State:/Network/Global/IPv4`'s
    /// `PrimaryService`, or `None` if there is no primary service (offline).
    fn primary_service(&self) -> Result<Option<String>, PlatformError>;

    /// The current `ServerAddresses` of `State:/Network/Service/<id>/DNS`.
    fn service_dns_servers(&self, service_dns_key: &str) -> Result<Vec<String>, PlatformError>;

    /// Run a `scutil` script (the text piped to `scutil`'s stdin).
    fn run_script(&self, script: &str) -> Result<(), PlatformError>;
}

/// Build the `scutil` script that sets `key`'s `ServerAddresses` to `servers`.
/// Replacing only the value we manage; SearchDomains and the rest of the dict
/// are not ours to keep — macOS repopulates a service's DNS dict from its source
/// (DHCP / the VPN) on the next network change, and the corp split-DNS is held
/// independently by the `/etc/resolver` files, so a minimal dict is sufficient
/// and avoids copying state we did not author.
pub(super) fn build_set_dns_script(key: &str, servers: &[String]) -> String {
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

    fn service_dns_servers(&self, service_dns_key: &str) -> Result<Vec<String>, PlatformError> {
        let dump = run_scutil(&format!("show {service_dns_key}\n"))?;
        Ok(crate::detector::macos_parse_array_field(
            &dump,
            "ServerAddresses",
        ))
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

    // Capture the prior state ONCE: only when no snapshot exists yet. A re-demote
    // (reconcile re-apply) must not snapshot our own fallback as the "prior".
    if snapshots.load().is_none() {
        let prior_servers = scutil.service_dns_servers(&key)?;
        snapshots.save(&DemoteSnapshot {
            service_dns_key: key.clone(),
            prior_servers,
        })?;
    }

    scutil.run_script(&build_set_dns_script(&key, fallback))?;
    log::info!("demoted the system default resolver to the off-tunnel fallback");
    Ok(true)
}

/// Restore the demoted default from the snapshot, then clear it. If the prior
/// service had explicit servers, set them back; if it had none, remove the key
/// so SystemConfiguration repopulates the service's DNS from its real source.
/// A missing snapshot is a clean no-op (nothing was demoted, or already
/// restored). Always clears the snapshot on success so a later demote re-snaps.
pub(super) fn restore(
    scutil: &dyn ScutilRunner,
    snapshots: &dyn SnapshotStore,
) -> Result<(), PlatformError> {
    let Some(snapshot) = snapshots.load() else {
        return Ok(()); // nothing demoted
    };
    let script = if snapshot.prior_servers.is_empty() {
        build_remove_key_script(&snapshot.service_dns_key)
    } else {
        build_set_dns_script(&snapshot.service_dns_key, &snapshot.prior_servers)
    };
    scutil.run_script(&script)?;
    snapshots.clear();
    log::info!("restored the system default resolver from the demote snapshot");
    Ok(())
}

/// Test doubles for the demote seam, shared by this module's tests and the
/// backend's apply/revert wiring tests.
#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use std::cell::RefCell;

    /// A fake `scutil` that captures every script run and returns canned state.
    pub(in super::super) struct FakeScutil {
        pub primary: Option<String>,
        pub prior_servers: Vec<String>,
        pub scripts: RefCell<Vec<String>>,
        pub fail_on_set: bool,
    }

    impl FakeScutil {
        /// A primary service exists with one prior resolver — the common case.
        pub fn up() -> Self {
            FakeScutil {
                primary: Some("ABC".to_string()),
                prior_servers: vec!["198.51.100.1".to_string()],
                scripts: RefCell::new(Vec::new()),
                fail_on_set: false,
            }
        }
    }

    impl ScutilRunner for FakeScutil {
        fn primary_service(&self) -> Result<Option<String>, PlatformError> {
            Ok(self.primary.clone())
        }
        fn service_dns_servers(&self, _key: &str) -> Result<Vec<String>, PlatformError> {
            Ok(self.prior_servers.clone())
        }
        fn run_script(&self, script: &str) -> Result<(), PlatformError> {
            if self.fail_on_set && script.contains("set ") {
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

    // --- pure script builders -------------------------------------------------

    #[test]
    fn set_dns_script_sets_server_addresses_for_the_key() {
        let script = build_set_dns_script(
            "State:/Network/Service/ABC/DNS",
            &["198.51.100.1".to_string()],
        );
        assert_eq!(
            script,
            "open\n\
             d.init\n\
             d.add ServerAddresses * 198.51.100.1\n\
             set State:/Network/Service/ABC/DNS\n\
             quit\n"
        );
    }

    #[test]
    fn set_dns_script_handles_multiple_servers() {
        let script = build_set_dns_script(
            "State:/Network/Service/ABC/DNS",
            &["198.51.100.1".to_string(), "198.51.100.2".to_string()],
        );
        assert!(script.contains("d.add ServerAddresses * 198.51.100.1 198.51.100.2\n"));
    }

    #[test]
    fn set_dns_script_with_no_servers_omits_the_add() {
        let script = build_set_dns_script("State:/Network/Service/ABC/DNS", &[]);
        assert!(!script.contains("d.add"));
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
        let snap = DemoteSnapshot {
            service_dns_key: "State:/Network/Service/ABC/DNS".to_string(),
            prior_servers: vec!["198.51.100.1".to_string(), "198.51.100.2".to_string()],
        };
        let parsed = DemoteSnapshot::deserialize(&snap.serialize()).unwrap();
        assert_eq!(parsed, snap);
    }

    #[test]
    fn snapshot_round_trips_with_no_prior_servers() {
        let snap = DemoteSnapshot {
            service_dns_key: "State:/Network/Service/ABC/DNS".to_string(),
            prior_servers: vec![],
        };
        let parsed = DemoteSnapshot::deserialize(&snap.serialize()).unwrap();
        assert_eq!(parsed, snap);
    }

    // --- demote / restore orchestration --------------------------------------

    #[test]
    fn demote_snapshots_prior_then_sets_the_fallback() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        let did = demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        assert!(did);
        // Prior state captured for the primary service.
        assert_eq!(
            snaps.load().unwrap(),
            DemoteSnapshot {
                service_dns_key: "State:/Network/Service/ABC/DNS".to_string(),
                prior_servers: vec!["198.51.100.1".to_string()],
            }
        );
        // Exactly one script issued: set the fallback on the primary service.
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("d.add ServerAddresses * 203.0.113.9\n"));
        assert!(scripts[0].contains("set State:/Network/Service/ABC/DNS\n"));
    }

    #[test]
    fn redemote_does_not_overwrite_the_original_snapshot() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        let after_first = snaps.load().unwrap();
        // A second demote (reconcile re-apply) must keep the ORIGINAL prior, not
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
    fn demote_with_no_primary_service_is_a_clean_noop() {
        let mut scutil = FakeScutil::up();
        scutil.primary = None;
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
    fn restore_sets_prior_servers_and_clears_snapshot() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(&scutil, &snaps, &["203.0.113.9".to_string()]).unwrap();
        scutil.scripts.borrow_mut().clear();

        restore(&scutil, &snaps).unwrap();
        // Restored the original servers...
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("d.add ServerAddresses * 198.51.100.1\n"));
        // ...and cleared the snapshot so a later demote re-snaps.
        drop(scripts);
        assert!(snaps.load().is_none());
    }

    #[test]
    fn restore_removes_the_key_when_no_prior_servers() {
        // A service with no explicit prior DNS: restore removes our override so
        // SC repopulates from the real source, rather than pinning empty.
        let mut scutil = FakeScutil::up();
        scutil.prior_servers = vec![];
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
        let mut scutil = FakeScutil::up();
        scutil.fail_on_set = true;
        let snaps = MemSnapshots::default();
        let result = demote(&scutil, &snaps, &["203.0.113.9".to_string()]);
        assert!(result.is_err());
        // Snapshot persisted so the half-done demote is recoverable on revert.
        assert!(snaps.load().is_some());
    }
}
