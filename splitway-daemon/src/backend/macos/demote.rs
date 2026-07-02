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
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use splitway_shared::platform::PlatformError;

use crate::detector::{macos_same_set as same_set, macos_scutil_script as scutil_script};

/// The dynamic-store key holding the primary network service id we demote.
const GLOBAL_IPV4_KEY: &str = "State:/Network/Global/IPv4";

/// The snapshot file: the pre-demote primary-service DNS, captured so revert can
/// restore it byte-for-byte even across an unclean daemon exit. Lives under the
/// daemon's runtime dir (root-owned, like the socket).
pub(super) const SNAPSHOT_PATH: &str = "/var/run/splitway/dns-demote.snapshot";

/// A captured pre-demote state: which service was demoted, the `InterfaceName`
/// it was bound to, and the `ServerAddresses` it had before.
///
/// Serialised to [`SNAPSHOT_PATH`] as JSON via `serde` (already a direct
/// dependency of this crate). JSON is used rather than a bespoke line format so
/// there is no hand-rolled parser to keep in sync and no silent-corruption edge
/// from a resolver value that happens to contain the field separator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The fallback resolver Splitway last installed on this service. Lets a
    /// same-service re-demote tell our own fallback (which we must NOT capture as
    /// the "prior") from a real DHCP update — even after a `fallback_dns` config
    /// change, where the service may still show a *previous* fallback we wrote.
    /// Updated only after a fallback write succeeds.
    pub installed_fallback: Vec<String>,
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

    /// Whether `key` currently exists in the dynamic store. A restore uses this
    /// to avoid re-creating a phantom `State:` entry for a service configd has
    /// already torn down (see [`restore_snapshot`]).
    fn key_exists(&self, key: &str) -> Result<bool, PlatformError>;

    /// Run a `scutil` *set* script (the text piped to `scutil`'s stdin). The impl
    /// must surface a command failure even when `scutil` reports it on stdout with
    /// a zero exit (see [`RealScutil::run_script`]).
    fn run_script(&self, script: &str) -> Result<(), PlatformError>;
}

/// Build the `scutil` script that overrides `key`'s `ServerAddresses`, leaving
/// every other field of the service's DNS dictionary intact.
///
/// `ServerAddresses` is the only field Splitway manages; the rest of the dict
/// (`SearchDomains`, `DomainName`, `SupplementalMatchDomains`, `SearchOrder`,
/// `InterfaceName`, …) belongs to DHCP / the VPN. So the script `get`s the
/// service's CURRENT dictionary into the working buffer first, then overrides
/// only `ServerAddresses`, then writes the merged dict back. A bare `d.init`
/// would instead start from an empty dictionary and the `set` would *replace* the
/// whole dict, dropping those unmanaged fields until the next network
/// reconfiguration repopulated them (breaking local search-domain / supplemental
/// DNS behaviour while Splitway is active). On a missing key `get` leaves the
/// (empty) initialised dict, so a fresh service is still created.
///
/// - `servers` non-empty → override `ServerAddresses` (demote to the fallback, or
///   restore the prior resolvers).
/// - `servers` empty → `d.remove ServerAddresses`, dropping only *our* override
///   while keeping the rest of the dict (restore of a service that had no explicit
///   prior `ServerAddresses`).
///
/// `InterfaceName` is preserved automatically by `get`; it is additionally
/// re-added when known as a belt-and-suspenders guarantee that the demoted
/// *physical* service stays identifiable to the detector (which anchors it by
/// `InterfaceName == PrimaryInterface`; see the detector's `decide`), even on the
/// missing-key path where `get` loaded nothing.
pub(super) fn build_set_dns_script(
    key: &str,
    servers: &[String],
    interface_name: Option<&str>,
) -> String {
    let mut script = String::new();
    script.push_str("open\n");
    script.push_str("d.init\n");
    // Load the live dict so unmanaged fields ride through unchanged.
    script.push_str(&format!("get {key}\n"));
    if !servers.is_empty() {
        script.push_str("d.add ServerAddresses *");
        for s in servers {
            script.push(' ');
            script.push_str(s);
        }
        script.push('\n');
    } else {
        // Drop only our ServerAddresses override; keep every other field.
        script.push_str("d.remove ServerAddresses\n");
    }
    if let Some(iface) = interface_name {
        // A single-value key (no `*`): the interface the service is bound to.
        script.push_str(&format!("d.add InterfaceName {iface}\n"));
    }
    script.push_str(&format!("set {key}\n"));
    script.push_str("quit\n");
    script
}

/// The `State:/Network/Service/<id>/DNS` key for a service id.
pub(super) fn service_dns_key(service_id: &str) -> String {
    format!("State:/Network/Service/{service_id}/DNS")
}

/// The real `scutil`-backed [`ScutilRunner`].
pub(super) struct RealScutil;

impl ScutilRunner for RealScutil {
    fn primary_service(&self) -> Result<Option<String>, PlatformError> {
        let dump = scutil_script(&format!("show {GLOBAL_IPV4_KEY}\n"))?;
        Ok(crate::detector::macos_parse_scalar_field(
            &dump,
            "PrimaryService",
        ))
    }

    fn service_dns_state(&self, service_dns_key: &str) -> Result<ServiceDnsState, PlatformError> {
        let dump = scutil_script(&format!("show {service_dns_key}\n"))?;
        Ok(ServiceDnsState {
            interface_name: crate::detector::macos_parse_scalar_field(&dump, "InterfaceName"),
            servers: crate::detector::macos_parse_array_field(&dump, "ServerAddresses"),
        })
    }

    fn key_exists(&self, key: &str) -> Result<bool, PlatformError> {
        // `scutil` prints `No such key` (exit 0) for a `show` of an absent key;
        // any other output means the key exists.
        Ok(!scutil_script(&format!("show {key}\n"))?.contains("No such key"))
    }

    fn run_script(&self, script: &str) -> Result<(), PlatformError> {
        // `scutil` in script mode reports a failed command on **stdout** while
        // still exiting 0 (it prints via `SCPrint`), so the exit-status check in
        // `scutil_script` alone would record a failed `set` — the exact leak this
        // demote closes — as success. A successful `open`/`d.init`/`get`/`d.add`/
        // `set`/`quit` sequence is silent, so any non-benign stdout is a failure;
        // the only benign output is `No such key` from `build_set_dns_script`'s
        // `get` on a first-time / missing service key.
        let out = scutil_script(script)?;
        if let Some(err) = scutil_set_error(&out) {
            return Err(PlatformError::CommandFailed(format!(
                "scutil reported an error running a set script (stdout, exit 0): {err}"
            )));
        }
        Ok(())
    }
}

/// Inspect `scutil` script-mode stdout for a reported command failure. Success is
/// silent, so this returns `Some(joined error lines)` for any non-empty output
/// other than the benign `No such key` notice (a `get` on a missing service key,
/// which the demote/restore scripts issue by design), else `None`. Pure so the
/// stdout-vs-exit-status contract is unit-tested without a live `scutil`.
fn scutil_set_error(stdout: &str) -> Option<String> {
    let residue: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains("No such key"))
        .collect();
    if residue.is_empty() {
        None
    } else {
        Some(residue.join("; "))
    }
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
        // A garbled/partial file parses to `None` (treated as "nothing demoted"),
        // never a panic.
        serde_json::from_str(&text).ok()
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
        let bytes = serde_json::to_vec(snapshot).map_err(|e| {
            PlatformError::CommandFailed(format!("serialising demote snapshot: {e}"))
        })?;
        // atomic_write keeps the snapshot intact on a crash mid-write.
        splitway_shared::config::atomic_write(&self.path, &bytes)
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
///
/// `corp_dns` is the VPN's own resolver (the servers the `/etc/resolver` scope
/// points at). Detection guarantees it differs from the physical resolver, so it
/// is never a legitimate "prior": passing it lets the same-service refresh reject
/// a `current` that equals it — i.e. a hijacking client that rewrote the physical
/// service's `ServerAddresses` to its corp DNS between samples — instead of
/// snapshotting the corp resolver as the off-VPN default a later restore installs.
pub(super) fn demote(
    scutil: &dyn ScutilRunner,
    snapshots: &dyn SnapshotStore,
    fallback: &[String],
    corp_dns: &[String],
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
    // - A snapshot for THIS SAME service → keep it, UNLESS the service's live DNS
    //   has changed to something other than our fallback (a DHCP renewal handed the
    //   same service a new resolver and the watcher re-emitted Up): then refresh
    //   the snapshot to the current resolver, so a later restore writes back the
    //   latest service DNS rather than the stale pre-change one. A reconcile
    //   re-apply where the service still shows our fallback keeps the snapshot (we
    //   must never capture our own fallback as the "prior").
    // - A snapshot for a DIFFERENT service (the primary changed while up — e.g. a
    //   Wi-Fi↔Ethernet switch) → restore that previous service from its snapshot
    //   first, so it is not left stranded on our fallback, then capture and demote
    //   the new primary. Exactly one service is ever demoted at a time.
    match snapshots.load() {
        Some(existing) if existing.service_dns_key == key => {
            let current = scutil.service_dns_state(&key)?;
            // Refresh the prior when the service's live DNS is a genuine new
            // resolver — a DHCP renewal handed the same service a new one and the
            // watcher re-emitted Up — so a later restore writes the latest rather
            // than a stale pre-change value. The hard part is telling that apart
            // from OUR OWN fallback:
            //
            // - `current != installed_fallback` — the recorded fallback we last
            //   wrote — is the primary signal. It stays correct in the default
            //   config where the fallback tracks the physical DHCP resolver (so a
            //   DHCP change makes `fallback` itself change): the new resolver still
            //   differs from the *previously* recorded `installed_fallback`.
            // - Comparing against the `fallback` we are ABOUT to install is only a
            //   backstop for when `installed_fallback` is empty (a first-time or
            //   lost record): then a `current` equal to that fallback is our own
            //   write, not a DHCP update. Applying this backstop unconditionally
            //   was the bug — in the default config the new DHCP resolver *equals*
            //   the new fallback, so it wrongly suppressed the refresh and pinned a
            //   stale prior. Gate it on `installed_fallback` being empty.
            // - `current != corp_dns` rejects the VPN's own resolver leaking onto
            //   the physical service (a hijacker rewrite between samples) — never a
            //   legitimate prior, since detection requires corp != physical.
            let installed_is_ours = same_set(&current.servers, &existing.installed_fallback)
                || (existing.installed_fallback.is_empty() && same_set(&current.servers, fallback));
            if !current.servers.is_empty()
                && !installed_is_ours
                && !same_set(&current.servers, corp_dns)
            {
                snapshots.save(&DemoteSnapshot {
                    service_dns_key: key.clone(),
                    interface_name: current.interface_name,
                    prior_servers: current.servers,
                    // Preserved here; the post-write step below reconciles it to
                    // the fallback actually installed.
                    installed_fallback: existing.installed_fallback,
                })?;
            }
            // else: keep the existing snapshot unchanged.
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

    // Record the fallback now on the service so the NEXT same-service demote
    // recognises it as ours, even across a `fallback_dns` change. Done AFTER a
    // successful write so a failed write leaves the previously-recorded fallback
    // (still the value on the service) intact.
    //
    // This save is BEST-EFFORT: the demote's load-bearing effects (the `set` above
    // and the prior snapshot) already succeeded, so a failure here must NOT fail
    // the apply. Propagating it would make `apply_with` roll back only the
    // `/etc/resolver` scope while the default stays demoted — the inverse
    // half-state (default off-tunnel, but corp domains no longer scoped, so they
    // leak via the fallback). The only thing lost is the `installed_fallback`
    // dedup hint, which the same-service branch above already tolerates being
    // stale/empty (the `installed_fallback.is_empty()` backstop). So log and keep
    // the demote applied.
    if let Some(mut snapshot) = snapshots.load() {
        if !same_set(&snapshot.installed_fallback, fallback) {
            snapshot.installed_fallback = fallback.to_vec();
            if let Err(e) = snapshots.save(&snapshot) {
                log::warn!(
                    "demote applied, but recording the installed fallback failed: {e}; \
                     a later re-demote will re-derive it"
                );
            }
        }
    }

    log::info!("demoted the system default resolver to the off-tunnel fallback");
    Ok(true)
}

/// Capture `key`'s current DNS dict (servers + interface name) into the
/// snapshot store, before it is overwritten by a demote. `installed_fallback` is
/// left empty here and set by the demote's post-write step once the fallback is
/// actually on the service.
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
        installed_fallback: Vec::new(),
    })
}

/// Restore the demoted default from the snapshot, then clear it. Returns whether
/// a snapshot was present and restored (so the caller can flush the DNS cache
/// even when no resolver files were removed). A missing snapshot is a clean no-op
/// → `Ok(false)`. Always clears the snapshot on success so a later demote re-snaps.
pub(super) fn restore(
    scutil: &dyn ScutilRunner,
    snapshots: &dyn SnapshotStore,
) -> Result<bool, PlatformError> {
    let Some(snapshot) = snapshots.load() else {
        return Ok(false); // nothing demoted
    };
    restore_snapshot(scutil, &snapshot)?;
    snapshots.clear();
    log::info!("restored the system default resolver from the demote snapshot");
    Ok(true)
}

/// Run the `scutil` write that restores one snapshot's service to its prior DNS.
/// Restores the prior `ServerAddresses` when there were any, otherwise removes
/// only our `ServerAddresses` override — either way `get`-preserving the rest of
/// the service's live DNS dict (so DHCP-provided SearchDomains/etc. survive the
/// round-trip). Does not touch the snapshot store — the caller owns clearing it.
fn restore_snapshot(
    scutil: &dyn ScutilRunner,
    snapshot: &DemoteSnapshot,
) -> Result<(), PlatformError> {
    // Skip the restore if the snapshot's service DNS key no longer exists — the
    // service departed entirely (e.g. Wi-Fi off → Ethernet in, configd deleted its
    // DNS/IPv4/IPv6 keys). `build_set_dns_script`'s `get`-then-`set` would re-create
    // the departed key as a phantom `State:/Network/Service/<old>/DNS` carrying the
    // stale resolver and (when the snapshot had no interface) no `InterfaceName` and
    // no live IPv4/IPv6 entity — which the detector reads as an unscoped
    // default-resolver hijacker → a false "VPN up" pointing corp domains at a dead
    // resolver. A gone service needs no restore; leaving it absent is correct.
    if !scutil.key_exists(&snapshot.service_dns_key)? {
        log::info!(
            "skipping restore of {}: its service DNS key no longer exists \
             (the service departed); not re-creating a phantom entry",
            snapshot.service_dns_key
        );
        return Ok(());
    }
    let script = build_set_dns_script(
        &snapshot.service_dns_key,
        &snapshot.prior_servers,
        snapshot.interface_name.as_deref(),
    );
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
        /// Service DNS keys the fake reports as absent (`key_exists` → false), so a
        /// test can model a service configd tore down (the phantom-restore case).
        pub absent_keys: RefCell<std::collections::HashSet<String>>,
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
                absent_keys: RefCell::new(std::collections::HashSet::new()),
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

        /// Mark a service DNS key as gone from the dynamic store, so `key_exists`
        /// reports it absent (the service departed — configd tore its keys down).
        pub fn set_key_absent(&self, key: &str) {
            self.absent_keys.borrow_mut().insert(key.to_string());
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
        fn key_exists(&self, key: &str) -> Result<bool, PlatformError> {
            Ok(!self.absent_keys.borrow().contains(key))
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
        /// When `Some(n)`, the next `n` saves succeed and every save after that
        /// fails — so a test can model the post-write `installed_fallback` record
        /// failing *after* the demote's `set` already took effect (the inverse
        /// half-state case). `None` = every save succeeds.
        pub ok_saves: std::cell::Cell<Option<usize>>,
    }
    impl SnapshotStore for MemSnapshots {
        fn load(&self) -> Option<DemoteSnapshot> {
            self.slot.borrow().clone()
        }
        fn save(&self, snapshot: &DemoteSnapshot) -> Result<(), PlatformError> {
            match self.ok_saves.get() {
                Some(0) => {
                    return Err(PlatformError::CommandFailed(
                        "simulated snapshot save failure".into(),
                    ))
                }
                Some(n) => self.ok_saves.set(Some(n - 1)),
                None => {}
            }
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

    fn snap(
        key: &str,
        iface: Option<&str>,
        servers: &[&str],
        installed: &[&str],
    ) -> DemoteSnapshot {
        DemoteSnapshot {
            service_dns_key: key.to_string(),
            interface_name: iface.map(str::to_string),
            prior_servers: servers.iter().map(|s| s.to_string()).collect(),
            installed_fallback: installed.iter().map(|s| s.to_string()).collect(),
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
             get State:/Network/Service/ABC/DNS\n\
             d.add ServerAddresses * 198.51.100.1\n\
             d.add InterfaceName en0\n\
             set State:/Network/Service/ABC/DNS\n\
             quit\n"
        );
    }

    #[test]
    fn set_dns_script_gets_the_live_dict_before_overriding_to_preserve_fields() {
        // The P2 invariant: `get` the existing dict BEFORE overriding
        // ServerAddresses so unmanaged DHCP/VPN fields (SearchDomains, DomainName,
        // SupplementalMatchDomains, …) ride through instead of being replaced by a
        // minimal dict.
        let script = build_set_dns_script(
            "State:/Network/Service/ABC/DNS",
            &["203.0.113.9".to_string()],
            Some("en0"),
        );
        let get_at = script.find("get State:/Network/Service/ABC/DNS\n").unwrap();
        let override_at = script.find("d.add ServerAddresses").unwrap();
        assert!(
            get_at < override_at,
            "must load the live dict with `get` before overriding ServerAddresses"
        );
        // No bare-init minimal write that would replace the whole dict.
        assert!(!script.contains("d.init\nd.add ServerAddresses"));
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
    fn set_dns_script_with_no_servers_removes_only_our_server_addresses() {
        // No prior servers → drop our ServerAddresses override (after `get`-ing the
        // live dict) rather than adding any, keeping every other field intact.
        let script = build_set_dns_script("State:/Network/Service/ABC/DNS", &[], Some("en0"));
        assert!(script.contains("get State:/Network/Service/ABC/DNS\n"));
        assert!(script.contains("d.remove ServerAddresses\n"));
        assert!(!script.contains("d.add ServerAddresses"));
        assert!(script.contains("d.add InterfaceName en0\n"));
        assert!(script.contains("set State:/Network/Service/ABC/DNS\n"));
    }

    #[test]
    fn service_dns_key_is_well_formed() {
        assert_eq!(
            service_dns_key("ABC-123"),
            "State:/Network/Service/ABC-123/DNS"
        );
    }

    /// Round-trip a snapshot through the on-disk (serde JSON) encoding.
    fn json_round_trip(s: &DemoteSnapshot) -> DemoteSnapshot {
        serde_json::from_str(&serde_json::to_string(s).unwrap()).unwrap()
    }

    #[test]
    fn snapshot_round_trips_through_disk_format() {
        let s = snap(
            "State:/Network/Service/ABC/DNS",
            Some("en0"),
            &["198.51.100.1", "198.51.100.2"],
            &["203.0.113.9", "203.0.113.10"],
        );
        assert_eq!(json_round_trip(&s), s);
    }

    #[test]
    fn snapshot_round_trips_with_no_prior_servers_and_no_interface() {
        let s = snap("State:/Network/Service/ABC/DNS", None, &[], &[]);
        assert_eq!(json_round_trip(&s), s);
    }

    #[test]
    fn snapshot_round_trips_with_an_interface_but_no_servers() {
        let s = snap(
            "State:/Network/Service/ABC/DNS",
            Some("en0"),
            &[],
            &["203.0.113.9"],
        );
        assert_eq!(json_round_trip(&s), s);
    }

    #[test]
    fn snapshot_deserialize_rejects_garbage() {
        // The store's `load` maps any deserialize error to `None` (treated as
        // "nothing demoted"), never a panic: empty, non-JSON, and JSON missing the
        // required `service_dns_key` field all fail to parse.
        assert!(serde_json::from_str::<DemoteSnapshot>("").is_err());
        assert!(serde_json::from_str::<DemoteSnapshot>("not json at all").is_err());
        assert!(
            serde_json::from_str::<DemoteSnapshot>("{\"prior_servers\":[\"192.0.2.1\"]}").is_err()
        );
    }

    // --- demote / restore orchestration --------------------------------------

    #[test]
    fn demote_snapshots_prior_then_sets_the_fallback_preserving_the_interface() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        let did = demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        assert!(did);
        // Prior state captured for the primary service, including its interface.
        assert_eq!(
            snaps.load().unwrap(),
            snap(
                "State:/Network/Service/ABC/DNS",
                Some("en0"),
                &["198.51.100.1"],
                &["203.0.113.9"],
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
    fn redemote_same_service_still_on_our_fallback_keeps_the_snapshot() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        let after_first = snaps.load().unwrap();
        // The demote took effect: the service now reports OUR fallback as its DNS.
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec!["203.0.113.9".to_string()],
            },
        );
        // A second demote (same primary, still our fallback) must keep the ORIGINAL
        // prior, not snapshot our own fallback.
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        assert_eq!(snaps.load().unwrap(), after_first);
        assert_eq!(
            after_first.prior_servers,
            vec!["198.51.100.1".to_string()],
            "the snapshot must remain the real prior resolver, never our fallback"
        );
    }

    #[test]
    fn redemote_same_service_refreshes_the_snapshot_when_the_dhcp_dns_changed() {
        // P2: the SAME primary service stays active but its DHCP resolver changes
        // (a new lease overwrites our fallback) and the watcher re-emits Up. The
        // snapshot must refresh to the NEW resolver, so a later restore writes back
        // the latest service DNS instead of pinning the pre-change one.
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        // DHCP hands the same service a new resolver.
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec!["198.51.100.77".to_string()],
            },
        );
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        assert_eq!(
            snaps.load().unwrap(),
            snap(
                "State:/Network/Service/ABC/DNS",
                Some("en0"),
                &["198.51.100.77"],
                &["203.0.113.9"],
            ),
            "the snapshot must track the new DHCP resolver so restore is not stale"
        );
    }

    #[test]
    fn redemote_after_a_fallback_change_keeps_the_original_prior() {
        // P2: `fallback_dns` changes from one resolver to another while demoted, so
        // the service still shows our PREVIOUS fallback. That previous fallback must
        // not be mistaken for a DHCP update and captured as the prior — restore must
        // still write the real DHCP resolver, not our old fallback.
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        // The demote took effect: the service shows our first fallback.
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec!["203.0.113.9".to_string()],
            },
        );
        // fallback_dns changes to a different resolver; re-demote.
        demote(
            &scutil,
            &snaps,
            &["203.0.113.50".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        let after = snaps.load().unwrap();
        assert_eq!(
            after.prior_servers,
            vec!["198.51.100.1".to_string()],
            "the prior must stay the real DHCP resolver, never our previous fallback"
        );
        assert_eq!(
            after.installed_fallback,
            vec!["203.0.113.50".to_string()],
            "the recorded installed fallback tracks the newly applied one"
        );
    }

    #[test]
    fn redemote_keeps_prior_when_the_installed_fallback_record_was_lost() {
        // P2: the post-write `installed_fallback` record can fail (e.g. a transient
        // `/var/run` write error) after the demote's `set` already succeeded, leaving
        // the snapshot's installed_fallback empty while the service already shows our
        // fallback. A retry with the SAME fallback must still recognise that value as
        // ours and keep the real DHCP prior — not snapshot our own fallback (which a
        // later restore would then write back as if it were the prior resolver).
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        // Seed the post-failure state: the real DHCP prior is captured, but the
        // installed_fallback record was lost (empty), and the service already shows
        // the fallback the previous `set` wrote.
        snaps
            .save(&snap(
                "State:/Network/Service/ABC/DNS",
                Some("en0"),
                &["198.51.100.1"],
                &[],
            ))
            .unwrap();
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec!["203.0.113.9".to_string()],
            },
        );
        // Retry the demote with the SAME fallback.
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        let after = snaps.load().unwrap();
        assert_eq!(
            after.prior_servers,
            vec!["198.51.100.1".to_string()],
            "a retry must not capture our own fallback as the prior when the \
             installed_fallback record was lost"
        );
        assert_eq!(
            after.installed_fallback,
            vec!["203.0.113.9".to_string()],
            "the post-write step repairs the lost installed_fallback record"
        );
    }

    #[test]
    fn redemote_default_config_refreshes_when_dhcp_equals_the_new_fallback() {
        // P1: in the DEFAULT config the fallback IS the physical DHCP resolver, so
        // a DHCP change makes the new resolver EQUAL the new fallback. The refresh
        // must still fire — keyed on the PREVIOUSLY recorded installed_fallback, not
        // on comparing against the fallback we are about to install — or a later
        // restore pins the stale pre-change resolver. The old `current != fallback`
        // guard wrongly suppressed exactly this case.
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        // First demote, default config: fallback == the physical DHCP resolver.
        demote(
            &scutil,
            &snaps,
            &["198.51.100.1".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        // DHCP renews: the service now shows a NEW resolver, and in the default
        // config the detector's demote-target (== the fallback) is that same value.
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec!["198.51.100.77".to_string()],
            },
        );
        demote(
            &scutil,
            &snaps,
            &["198.51.100.77".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        let after = snaps.load().unwrap();
        assert_eq!(
            after.prior_servers,
            vec!["198.51.100.77".to_string()],
            "the prior must refresh to the new DHCP resolver even when it equals the new fallback"
        );
        assert_eq!(after.installed_fallback, vec!["198.51.100.77".to_string()]);
    }

    #[test]
    fn redemote_never_captures_the_corp_resolver_as_a_prior() {
        // P1 edge: a hijacking client rewrites the physical service's
        // ServerAddresses to its OWN corp DNS between samples. That corp value must
        // never be captured as the prior — a later restore would then install the
        // corp resolver as the off-VPN default. The real prior stays untouched.
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        // The physical service now (transiently) shows the corp resolver.
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec!["192.0.2.53".to_string()],
            },
        );
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        assert_eq!(
            snaps.load().unwrap().prior_servers,
            vec!["198.51.100.1".to_string()],
            "the corp resolver must never be snapshotted as the physical prior"
        );
    }

    #[test]
    fn demote_survives_a_post_write_installed_fallback_save_failure() {
        // P2: the `set` already demoted the default and the prior snapshot is saved,
        // but the post-write installed_fallback record fails. The demote must NOT
        // fail — that would make apply_with roll back only the /etc/resolver scope,
        // leaving the inverse half-state (default demoted, corp domains unscoped →
        // they leak via the fallback). It stays applied; only the dedup hint is lost.
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        // The first save (capture_snapshot) succeeds; the second (post-write) fails.
        snaps.ok_saves.set(Some(1));
        let did = demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        assert!(
            did,
            "the demote stays applied despite the bookkeeping save failure"
        );
        // The set took effect (the fallback script was issued)...
        assert!(scutil
            .scripts
            .borrow()
            .iter()
            .any(|s| s.contains("d.add ServerAddresses * 203.0.113.9")));
        // ...and the prior snapshot survived (recoverable on a later revert).
        let after = snaps.load().unwrap();
        assert_eq!(after.prior_servers, vec!["198.51.100.1".to_string()]);
        // The installed_fallback hint was not recorded (its save failed) — tolerated
        // by the same-service refresh branch's `installed_fallback.is_empty()` path.
        assert!(after.installed_fallback.is_empty());
    }

    #[test]
    fn restore_skips_a_departed_service_instead_of_recreating_a_phantom() {
        // P1: on VPN-down the demoted service has departed (Wi-Fi off → Ethernet in;
        // configd tore down its keys). Restore must NOT re-create the key — a
        // get-then-set would leave a phantom the detector reads as an unscoped
        // default-resolver hijacker → a false "VPN up" at a dead resolver.
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        scutil.scripts.borrow_mut().clear();
        // The demoted service's DNS key is gone.
        scutil.set_key_absent("State:/Network/Service/ABC/DNS");

        let restored = restore(&scutil, &snaps).unwrap();
        assert!(
            restored,
            "restore reports it handled the snapshot so the caller still flushes"
        );
        // No set script re-created the departed key...
        assert!(
            scutil
                .scripts
                .borrow()
                .iter()
                .all(|s| !s.contains("set State:/Network/Service/ABC/DNS")),
            "a departed service must not be re-created as a phantom"
        );
        // ...and the snapshot is cleared.
        assert!(snaps.load().is_none());
    }

    #[test]
    fn demote_on_a_changed_primary_skips_a_departed_old_service() {
        // P1: the primary switched because the OLD service departed entirely (its
        // keys are gone). The old service must NOT be restored (no phantom); the new
        // primary is still snapshotted and demoted.
        let scutil = FakeScutil::up(); // primary ABC (en0)
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        scutil.scripts.borrow_mut().clear();
        // The old primary ABC departed; the new primary is XYZ (en1) with its own DNS.
        scutil.set_key_absent("State:/Network/Service/ABC/DNS");
        scutil.set_primary("XYZ");
        scutil.set_service_state(
            "State:/Network/Service/XYZ/DNS",
            ServiceDnsState {
                interface_name: Some("en1".to_string()),
                servers: vec!["198.51.100.50".to_string()],
            },
        );
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();

        let scripts = scutil.scripts.borrow();
        // The departed old service is NOT restored (no phantom re-created).
        assert!(
            scripts
                .iter()
                .all(|s| !s.contains("set State:/Network/Service/ABC/DNS")),
            "the departed old service must not be re-created"
        );
        // The new primary IS demoted to the fallback.
        assert!(scripts
            .iter()
            .any(|s| s.contains("set State:/Network/Service/XYZ/DNS")
                && s.contains("d.add ServerAddresses * 203.0.113.9")));
        drop(scripts);
        assert_eq!(
            snaps.load().unwrap().service_dns_key,
            "State:/Network/Service/XYZ/DNS"
        );
    }

    #[test]
    fn scutil_set_error_flags_stdout_failures_but_allows_no_such_key() {
        // Success is silent → None.
        assert!(scutil_set_error("").is_none());
        assert!(scutil_set_error("\n  \n").is_none());
        // The benign `get` on a missing key → None (the demote/restore scripts
        // issue `get` by design, and a first-time service key does not exist yet).
        assert!(scutil_set_error("  No such key\n").is_none());
        assert!(scutil_set_error("No such key\nNo such key\n").is_none());
        // A real stdout-reported failure (exit 0) → Some.
        let err = scutil_set_error("  SCPreferencesCommitChanges: Permission denied\n").unwrap();
        assert!(err.contains("Permission denied"));
        // Mixed: only the non-benign line is reported.
        let err = scutil_set_error("No such key\n  failed to apply\n").unwrap();
        assert!(err.contains("failed to apply") && !err.contains("No such key"));
    }

    #[test]
    fn demote_on_a_changed_primary_restores_the_old_then_snapshots_the_new() {
        // P2: the primary service changes while up (e.g. Wi-Fi → Ethernet) with a
        // snapshot already present. The old service must be un-demoted (not left
        // stranded on our fallback), and the NEW service snapshotted + demoted.
        let scutil = FakeScutil::up(); // primary = ABC (en0)
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
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
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();

        // The snapshot now tracks the NEW primary with its real prior DNS.
        assert_eq!(
            snaps.load().unwrap(),
            snap(
                "State:/Network/Service/XYZ/DNS",
                Some("en1"),
                &["198.51.100.50"],
                &["203.0.113.9"],
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
        let did = demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        assert!(!did);
        assert!(snaps.load().is_none());
        assert!(scutil.scripts.borrow().is_empty());
    }

    #[test]
    fn demote_to_empty_fallback_is_rejected() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        assert!(demote(&scutil, &snaps, &[], &["192.0.2.53".to_string()]).is_err());
        // Nothing captured or issued on rejection.
        assert!(snaps.load().is_none());
        assert!(scutil.scripts.borrow().is_empty());
    }

    #[test]
    fn restore_sets_prior_servers_with_the_interface_and_clears_snapshot() {
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
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
    fn restore_clears_only_our_servers_when_there_were_no_prior_ones() {
        // A service with no explicit prior ServerAddresses: restore drops just our
        // override (d.remove ServerAddresses) after `get`-ing the live dict, so any
        // DHCP-provided SearchDomains/etc. are preserved rather than wiped.
        let scutil = FakeScutil::up();
        scutil.set_service_state(
            "State:/Network/Service/ABC/DNS",
            ServiceDnsState {
                interface_name: Some("en0".to_string()),
                servers: vec![],
            },
        );
        let snaps = MemSnapshots::default();
        demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        )
        .unwrap();
        scutil.scripts.borrow_mut().clear();

        restore(&scutil, &snaps).unwrap();
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("get State:/Network/Service/ABC/DNS\n"));
        assert!(scripts[0].contains("d.remove ServerAddresses\n"));
        assert!(!scripts[0].contains("d.add ServerAddresses"));
        assert!(scripts[0].contains("set State:/Network/Service/ABC/DNS\n"));
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
        let result = demote(
            &scutil,
            &snaps,
            &["203.0.113.9".to_string()],
            &["192.0.2.53".to_string()],
        );
        assert!(result.is_err());
        // Snapshot persisted so the half-done demote is recoverable on revert.
        assert!(snaps.load().is_some());
    }
}
