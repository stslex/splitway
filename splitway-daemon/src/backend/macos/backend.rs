//! The macOS [`DnsBackend`]: writes `/etc/resolver/<domain>` files and flushes
//! the system DNS cache. Transactional like the Linux backend — a failed apply
//! never leaves a partial split-DNS set behind — and revert only removes files
//! Splitway itself wrote (ownership via [`resolver::is_managed`]).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use splitway_shared::config::atomic_write;
use splitway_shared::ipc::{LinkDnsState, ResolutionInfo};
use splitway_shared::platform::{DnsBackend, PlatformError, VpnInfo};

use super::demote;
use super::resolver::{is_managed, is_valid_domain, resolver_contents, resolver_path};
use super::MacosBackend;

/// macOS reads per-domain resolvers from here (`man 5 resolver`).
const RESOLVER_DIR: &str = "/etc/resolver";

impl DnsBackend for MacosBackend {
    fn apply_rules(&self, vpn_info: &VpnInfo, domains: &[String]) -> Result<(), PlatformError> {
        if vpn_info.dns_servers.is_empty() {
            return Err(PlatformError::CommandFailed(
                "no DNS servers in VpnInfo".to_string(),
            ));
        }
        let fallback = vpn_info.demote_target.as_deref().filter(|f| !f.is_empty());
        apply_with(
            Path::new(RESOLVER_DIR),
            &vpn_info.dns_servers,
            domains,
            fallback,
            &demote::RealScutil,
            &demote::FileSnapshotStore::new(),
        )?;
        flush_dns_cache();
        Ok(())
    }

    fn revert_rules(&self, _interface: &str) -> Result<(), PlatformError> {
        let removed = revert_with(
            Path::new(RESOLVER_DIR),
            &demote::RealScutil,
            &demote::FileSnapshotStore::new(),
        )?;
        if removed > 0 {
            flush_dns_cache();
        }
        log::info!("reverted {removed} splitway resolver file(s) and restored any demoted default");
        Ok(())
    }

    /// Best-effort live read-back. macOS has no per-link DNS block to parse
    /// (`resolvectl status`'s analogue), and `scutil --dns` does not attribute
    /// state to the interface that owns it — so the authoritative "what Splitway
    /// installed" is the managed `/etc/resolver/<domain>` files this backend
    /// wrote. We reconstruct the live state from exactly those: each managed
    /// file's name is a routing domain, and its `nameserver` lines are the
    /// servers. The `interface` argument is advisory (resolver files are keyed by
    /// domain, not interface). Verify on hardware before relying on this.
    fn read_link_state(&self, _interface: &str) -> Result<LinkDnsState, PlatformError> {
        Ok(read_managed_state(Path::new(RESOLVER_DIR)))
    }

    fn reverts_globally(&self) -> bool {
        // revert_rules removes every managed resolver file, not just one
        // interface's — resolver files are keyed by domain (see revert_rules).
        true
    }

    /// Best-effort live resolution. macOS routes the lookup through the system
    /// resolver, which honors the `/etc/resolver/<domain>` files this backend
    /// writes — so a covered domain resolves via the VPN's DNS. But the system
    /// lookup does not attribute which link/resolver answered, so `via_interface`
    /// and `via_dns` are always `None` (unlike Linux's strong attribution). This
    /// reports the resolved address, not reachability (see the trait doc).
    fn resolve(&self, host: &str) -> Result<ResolutionInfo, PlatformError> {
        use std::net::ToSocketAddrs;

        // Port 0: we only want the address resolution, not a connection. This
        // goes through getaddrinfo, which respects `/etc/resolver`.
        let addresses: Vec<String> = (host, 0u16)
            .to_socket_addrs()
            .map_err(|e| PlatformError::CommandFailed(format!("resolve {host}: {e}")))?
            .map(|addr| addr.ip().to_string())
            // Dedup while keeping output stable: getaddrinfo returns A/AAAA
            // records, and a host often resolves to the same IP via several
            // entries (duplicate records, or both the v4 and v6 family).
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        if addresses.is_empty() {
            return Err(PlatformError::ParseError(format!(
                "no addresses resolved for {host}"
            )));
        }

        Ok(ResolutionInfo {
            addresses,
            via_interface: None,
            via_dns: None,
        })
    }
}

/// The apply pipeline, parameterized over the resolver dir and the demote seam
/// so the scope+demote wiring is unit-testable without touching the live system.
/// Two steps, transactional across both:
///
/// 1. **Scope** — `apply_to_dir` routes `domains` to `servers` via
///    `/etc/resolver` (itself transactional; rolls back on any failure).
/// 2. **Demote** — when `fallback` is `Some` (the VPN hijacked the system
///    default), [`demote::demote`] sends non-corp DNS off-tunnel. If it fails,
///    the resolver scope from step 1 is rolled back so the system is never left
///    half-changed (scoped but with the default still hijacked), and the demote
///    error is surfaced. The demote's own snapshot still lets a later revert
///    restore the default even if this rollback is incomplete.
fn apply_with(
    dir: &Path,
    servers: &[String],
    domains: &[String],
    fallback: Option<&[String]>,
    scutil: &dyn demote::ScutilRunner,
    snapshots: &dyn demote::SnapshotStore,
) -> Result<(), PlatformError> {
    apply_to_dir(dir, servers, domains)?;

    if let Some(fallback) = fallback {
        if let Err(e) = demote::demote(scutil, snapshots, fallback) {
            // Undo the scope just written so no partial state remains.
            if let Err(rollback_err) = remove_managed(dir, None) {
                log::error!(
                    "demote failed and rolling back the resolver scope also failed: {rollback_err}"
                );
            }
            return Err(e);
        }
    }
    Ok(())
}

/// The revert pipeline, parameterized over the resolver dir and the demote seam.
/// Removes every managed `/etc/resolver` file AND restores any demoted system
/// default from the on-disk snapshot. Returns the number of resolver files
/// removed (so the caller can decide whether a cache flush is warranted).
///
/// The restore runs even when no resolver files were removed: a prior run may
/// have demoted without (or after pruning) resolver files, and the snapshot is
/// the record of record. A restore failure is surfaced (not swallowed) so the
/// caller retries rather than recording a clean revert over a still-demoted
/// default.
fn revert_with(
    dir: &Path,
    scutil: &dyn demote::ScutilRunner,
    snapshots: &dyn demote::SnapshotStore,
) -> Result<usize, PlatformError> {
    let removed = remove_managed(dir, None).map_err(|e| {
        PlatformError::CommandFailed(format!("failed to remove resolver files: {e}"))
    })?;
    demote::restore(scutil, snapshots)?;
    Ok(removed)
}

/// Prior on-disk state of a resolver file before this apply touched it, used to
/// undo a failed apply.
enum Prior {
    /// The file existed; these are its bytes, restored on rollback.
    Existed(Vec<u8>),
    /// The file did not exist; removed on rollback.
    Absent,
}

/// Reconcile the resolver files in `dir` to exactly `domains`, each pointing at
/// `servers`. A target path that already exists *without* our marker is the
/// user's own resolver: we refuse to overwrite it (replacing it would let a
/// later revert delete the user's config). Transactional and **non-destructive
/// on failure**: each target file's prior state is captured before it is
/// overwritten, so a mid-write failure restores overwritten files to their
/// original bytes and removes only the files this call newly created. A failed
/// re-apply therefore leaves the previously-live split-DNS exactly as it was —
/// never a partial or empty set. Then prunes our now-unwanted files; a prune
/// failure is returned (not swallowed) so the caller retries rather than
/// recording success while a dropped domain's file keeps routing.
fn apply_to_dir(dir: &Path, servers: &[String], domains: &[String]) -> Result<(), PlatformError> {
    // Reject names that are not safe single path components before touching the
    // filesystem: `dir.join("../x")` would escape /etc/resolver, and a
    // slash-containing name would never match the single-component filenames
    // prune compares against.
    for domain in domains {
        if !is_valid_domain(domain) {
            return Err(PlatformError::CommandFailed(format!(
                "refusing to apply invalid domain name: {domain:?}"
            )));
        }
    }

    let contents = resolver_contents(servers);

    let mut written: Vec<(PathBuf, Prior)> = Vec::with_capacity(domains.len());
    for domain in domains {
        let path = resolver_path(dir, domain);
        // Snapshot the prior state so a later failure can restore it exactly.
        // symlink_metadata does not follow links, so we classify the entry
        // itself, not a symlink target.
        let prior = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                let bytes = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    // Present but unreadable: don't risk clobbering it.
                    Err(e) => {
                        rollback(&written);
                        return Err(PlatformError::CommandFailed(format!(
                            "failed to read resolver file {} before overwrite: {e}",
                            path.display()
                        )));
                    }
                };
                // Only overwrite a resolver file we previously wrote. If one
                // exists without our marker it is the user's: refuse rather than
                // replace it, because revert would later delete it (it now
                // carries our marker) and the user's original DNS config would
                // be lost — the in-call snapshot does not survive a successful
                // apply.
                if !is_managed(&String::from_utf8_lossy(&bytes)) {
                    rollback(&written);
                    return Err(PlatformError::CommandFailed(format!(
                        "refusing to overwrite resolver file not created by splitway: {}",
                        path.display()
                    )));
                }
                Prior::Existed(bytes)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Prior::Absent,
            // Exists but is a symlink / directory / other non-regular entry:
            // refuse. We never follow a symlink (rollback could not restore it,
            // and the write would replace the link with a regular file).
            Ok(_) => {
                rollback(&written);
                return Err(PlatformError::CommandFailed(format!(
                    "refusing to overwrite non-regular resolver entry: {}",
                    path.display()
                )));
            }
            Err(e) => {
                rollback(&written);
                return Err(PlatformError::CommandFailed(format!(
                    "failed to stat resolver file {} before overwrite: {e}",
                    path.display()
                )));
            }
        };
        // atomic_write is itself atomic, so on failure `path` is untouched
        // (still in its `prior` state) and only the earlier writes need undoing.
        if let Err(e) = atomic_write(&path, contents.as_bytes()) {
            rollback(&written);
            return Err(PlatformError::CommandFailed(format!(
                "failed to write resolver file {}: {e}; rolled back {} file(s)",
                path.display(),
                written.len()
            )));
        }
        written.push((path, prior));
    }

    let keep: BTreeSet<&str> = domains.iter().map(String::as_str).collect();
    // Surface prune failures: a leftover file for a dropped domain keeps routing
    // it through the VPN. Roll back this call's writes first — on a failed
    // reconcile the state machine does not record `applied`, so newly-created
    // files left behind would be skipped by a later revert. This undoes only
    // this call's writes; a partially-completed prune is not un-deleted, but
    // prune only ever removes files that are being dropped or are already stale,
    // so those deletions are safe to leave. The result stays consistent with the
    // retained `applied` state.
    if let Err(e) = remove_managed(dir, Some(&keep)) {
        rollback(&written);
        return Err(PlatformError::CommandFailed(format!(
            "failed to prune stale resolver files: {e}"
        )));
    }
    Ok(())
}

/// Undo the writes recorded in `written`, most recent first: restore each
/// overwritten file to its prior bytes and remove files this call created.
fn rollback(written: &[(PathBuf, Prior)]) {
    for (path, prior) in written.iter().rev() {
        match prior {
            Prior::Existed(bytes) => {
                if let Err(e) = atomic_write(path, bytes) {
                    log::error!("rollback could not restore {}: {e}", path.display());
                }
            }
            Prior::Absent => {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Remove every Splitway-owned resolver file in `dir`, except those whose
/// filename is in `keep` (when `Some`). A missing `dir` is treated as "nothing
/// to remove". Returns how many files were removed. Files we do not own (no
/// marker) and unreadable files are left untouched.
fn remove_managed(dir: &Path, keep: Option<&BTreeSet<&str>>) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut removed = 0;
    for entry in entries {
        let path = entry?.path();
        // Only ever consider real files we could have written. `symlink_metadata`
        // does not follow links, so a symlink — even one pointing at a
        // marker-prefixed file — is skipped rather than read through and
        // classified as managed.
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if keep.is_some_and(|keep| keep.contains(name)) {
            continue;
        }
        // Only touch files we wrote; never a user-authored resolver file.
        match fs::read_to_string(&path) {
            Ok(contents) if is_managed(&contents) => {
                fs::remove_file(&path)?;
                removed += 1;
            }
            _ => {}
        }
    }
    Ok(removed)
}

/// Reconstruct the live [`LinkDnsState`] from the managed `/etc/resolver` files
/// in `dir`: each managed file's name is a routing domain and its `nameserver`
/// lines are the servers (`man 5 resolver`). Files we do not own (no marker),
/// unreadable files, and non-regular entries (symlinks, directories) are skipped.
/// Best-effort: a missing dir or a read error degrades to an empty state rather
/// than failing, which the caller treats as "read-back unavailable". Entries are
/// visited in sorted order for a stable result, and servers are de-duplicated
/// across files (every managed file lists the same VPN servers).
fn read_managed_state(dir: &Path) -> LinkDnsState {
    let mut routing_domains: Vec<String> = Vec::new();
    let mut servers: Vec<String> = Vec::new();

    let mut entries: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return LinkDnsState::default(),
    };
    entries.sort();

    for path in entries {
        // Only ever read real files we could have written — `symlink_metadata`
        // does not follow links, so a symlink is skipped rather than read through.
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        // Only attribute files Splitway wrote; a user-authored resolver is not
        // part of the daemon's belief and would be a false "live" entry.
        if !is_managed(&contents) {
            continue;
        }
        // The filename is the routing domain it resolves.
        if !routing_domains.iter().any(|d| d == name) {
            routing_domains.push(name.to_string());
        }
        for line in contents.lines() {
            if let Some(server) = line.trim().strip_prefix("nameserver ") {
                let server = server.trim();
                if !server.is_empty() && !servers.iter().any(|s| s == server) {
                    servers.push(server.to_string());
                }
            }
        }
    }

    LinkDnsState {
        servers,
        routing_domains,
        // macOS split-DNS is per-domain (`/etc/resolver/<domain>` files): there is
        // no link-level catch-all to leak through, so the default-route flag does
        // not apply here. `None` keeps `compare_drift`'s leak check from ever
        // tripping on macOS.
        default_route: None,
    }
}

/// Best-effort flush of the macOS DNS caches after resolver files change.
/// Failures are logged, not fatal: the resolver files are already correct and
/// the cache expires on its own.
fn flush_dns_cache() {
    run_best_effort("dscacheutil", &["-flushcache"]);
    run_best_effort("killall", &["-HUP", "mDNSResponder"]);
}

fn run_best_effort(cmd: &str, args: &[&str]) {
    match Command::new(cmd).args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => log::warn!(
            "{cmd} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => log::warn!("could not run {cmd}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "splitway-resolver-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn servers() -> Vec<String> {
        vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()]
    }

    // --- scope + demote wiring (apply_with / revert_with) --------------------
    //
    // These exercise the two-step pipeline with the demote seam faked, so they
    // assert BOTH the /etc/resolver files written and the exact scutil script
    // issued — without touching the live system.

    use super::demote::test_support::{FakeScutil, MemSnapshots};

    #[test]
    fn apply_with_scopes_and_demotes_to_the_fallback() {
        let dir = temp_dir("apply-with-demote");
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        let corp = vec!["192.0.2.53".to_string()];
        let fallback = vec!["198.51.100.1".to_string()];

        apply_with(
            &dir,
            &corp,
            &[
                "corp.example.com".to_string(),
                "jira.corp.example.com".to_string(),
            ],
            Some(&fallback),
            &scutil,
            &snaps,
        )
        .unwrap();

        // Scope: a managed resolver file per corp domain, pointing at corp DNS.
        for domain in ["corp.example.com", "jira.corp.example.com"] {
            let body = fs::read_to_string(dir.join(domain)).unwrap();
            assert!(is_managed(&body));
            assert!(body.contains("nameserver 192.0.2.53"));
        }
        // Demote: exactly the set-fallback script on the primary service, and
        // the prior default snapshotted for restore.
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("d.add ServerAddresses * 198.51.100.1\n"));
        assert!(scripts[0].contains("set State:/Network/Service/ABC/DNS\n"));
        drop(scripts);
        assert_eq!(
            snaps.slot.borrow().as_ref().unwrap().prior_servers,
            vec!["198.51.100.1".to_string()]
        );
    }

    #[test]
    fn apply_with_no_fallback_scopes_only_no_demote() {
        let dir = temp_dir("apply-scope-only");
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        apply_with(
            &dir,
            &["192.0.2.53".to_string()],
            &["corp.example.com".to_string()],
            None,
            &scutil,
            &snaps,
        )
        .unwrap();
        assert!(dir.join("corp.example.com").exists());
        // No demote issued, nothing snapshotted.
        assert!(scutil.scripts.borrow().is_empty());
        assert!(snaps.slot.borrow().is_none());
    }

    #[test]
    fn apply_with_rolls_back_the_scope_when_the_demote_fails() {
        // Transactional across both steps: a demote failure must undo the
        // resolver scope so the system is never left half-changed.
        let dir = temp_dir("apply-rollback");
        let mut scutil = FakeScutil::up();
        scutil.fail_on_set = true;
        let snaps = MemSnapshots::default();

        let result = apply_with(
            &dir,
            &["192.0.2.53".to_string()],
            &["corp.example.com".to_string()],
            Some(&["198.51.100.1".to_string()]),
            &scutil,
            &snaps,
        );
        assert!(result.is_err(), "a demote failure fails the whole apply");
        // The scope was rolled back — no managed resolver file remains.
        assert!(
            !dir.join("corp.example.com").exists(),
            "the resolver scope must be rolled back on demote failure"
        );
    }

    #[test]
    fn revert_with_removes_files_and_restores_the_default() {
        let dir = temp_dir("revert-with");
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        // Apply (scope + demote), then revert.
        apply_with(
            &dir,
            &["192.0.2.53".to_string()],
            &["corp.example.com".to_string()],
            Some(&["198.51.100.1".to_string()]),
            &scutil,
            &snaps,
        )
        .unwrap();
        scutil.scripts.borrow_mut().clear();

        let removed = revert_with(&dir, &scutil, &snaps).unwrap();
        assert_eq!(removed, 1, "the one managed resolver file is removed");
        assert!(!dir.join("corp.example.com").exists());
        // The default was restored to the snapshotted prior servers, and the
        // snapshot cleared.
        let scripts = scutil.scripts.borrow();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("d.add ServerAddresses * 198.51.100.1\n"));
        drop(scripts);
        assert!(snaps.slot.borrow().is_none());
    }

    #[test]
    fn revert_with_nothing_applied_is_a_clean_noop() {
        let dir = temp_dir("revert-noop");
        let scutil = FakeScutil::up();
        let snaps = MemSnapshots::default();
        let removed = revert_with(&dir, &scutil, &snaps).unwrap();
        assert_eq!(removed, 0);
        assert!(scutil.scripts.borrow().is_empty());
    }

    #[test]
    fn apply_writes_one_marked_file_per_domain() {
        let dir = temp_dir("apply-writes");
        apply_to_dir(
            &dir,
            &servers(),
            &["a.com".to_string(), "b.com".to_string()],
        )
        .unwrap();

        for domain in ["a.com", "b.com"] {
            let body = fs::read_to_string(dir.join(domain)).unwrap();
            assert!(is_managed(&body), "{domain} should be marked ours");
            assert!(body.contains("nameserver 10.0.0.1"));
            assert!(body.contains("nameserver 10.0.0.2"));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_creates_missing_resolver_dir() {
        let parent = temp_dir("apply-mkdir");
        let dir = parent.join("resolver"); // does not exist yet
        apply_to_dir(&dir, &servers(), &["a.com".to_string()]).unwrap();
        assert!(dir.join("a.com").is_file());
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn reapply_with_fewer_domains_prunes_the_dropped_one() {
        let dir = temp_dir("apply-prune");
        apply_to_dir(
            &dir,
            &servers(),
            &["a.com".to_string(), "b.com".to_string()],
        )
        .unwrap();
        // Re-apply with b.com dropped: its file must be pruned, a.com kept.
        apply_to_dir(&dir, &servers(), &["a.com".to_string()]).unwrap();
        assert!(dir.join("a.com").is_file());
        assert!(!dir.join("b.com").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn revert_removes_only_files_we_own() {
        let dir = temp_dir("revert-owned");
        apply_to_dir(&dir, &servers(), &["a.com".to_string()]).unwrap();
        // A resolver file the user wrote by hand.
        let user_file = dir.join("user.example");
        fs::write(&user_file, "nameserver 9.9.9.9\n").unwrap();

        let removed = remove_managed(&dir, None).unwrap();
        assert_eq!(removed, 1);
        assert!(!dir.join("a.com").exists(), "our file is removed");
        assert!(user_file.exists(), "the user's file is untouched");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn revert_on_missing_dir_is_noop() {
        let mut missing = std::env::temp_dir();
        missing.push(format!("splitway-absent-resolver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&missing);
        assert_eq!(remove_managed(&missing, None).unwrap(), 0);
    }

    #[test]
    fn failed_write_rolls_back_files_written_in_this_call() {
        // Make the second domain unwritable by pre-creating it as a directory:
        // atomic_write renames a temp file over the path, which fails on a dir.
        let dir = temp_dir("apply-rollback");
        fs::create_dir(dir.join("b.com")).unwrap();

        let err = apply_to_dir(
            &dir,
            &servers(),
            &["a.com".to_string(), "b.com".to_string()],
        )
        .unwrap_err();
        assert!(matches!(err, PlatformError::CommandFailed(_)));
        // a.com was newly created then rolled back when b.com failed: no
        // partial split-DNS set remains.
        assert!(!dir.join("a.com").exists(), "a.com must be rolled back");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_reapply_restores_previously_live_files() {
        let dir = temp_dir("reapply-rollback");
        // Establish a live set with the original servers.
        let v1 = vec!["10.0.0.1".to_string()];
        apply_to_dir(&dir, &v1, &["a.com".to_string(), "b.com".to_string()]).unwrap();
        let a_before = fs::read_to_string(dir.join("a.com")).unwrap();

        // Force a re-apply (with NEW servers) to fail at c.com — a directory
        // cannot be atomically overwritten by a file.
        fs::create_dir(dir.join("c.com")).unwrap();
        let v2 = vec!["10.9.9.9".to_string()];
        let err = apply_to_dir(
            &dir,
            &v2,
            &[
                "a.com".to_string(),
                "b.com".to_string(),
                "c.com".to_string(),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, PlatformError::CommandFailed(_)));

        // The previously-live files survive with their ORIGINAL contents — a
        // failed re-apply is non-destructive, not a partial/empty wipe.
        assert_eq!(fs::read_to_string(dir.join("a.com")).unwrap(), a_before);
        assert!(dir.join("b.com").is_file());
        assert!(a_before.contains("nameserver 10.0.0.1"));
        assert!(!a_before.contains("10.9.9.9"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_refuses_to_overwrite_a_user_authored_resolver() {
        let dir = temp_dir("apply-refuse-unmanaged");
        // The user already has a hand-written resolver for this domain.
        let user = dir.join("corp.example.com");
        fs::write(&user, "nameserver 9.9.9.9\n").unwrap();

        let err = apply_to_dir(&dir, &servers(), &["corp.example.com".to_string()]).unwrap_err();
        assert!(matches!(err, PlatformError::CommandFailed(_)));
        // The user's file is left exactly as it was — not replaced by a managed
        // file that a later revert would delete.
        assert_eq!(fs::read_to_string(&user).unwrap(), "nameserver 9.9.9.9\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_refuses_to_overwrite_a_symlink() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("apply-refuse-symlink");
        // A resolver entry that is a symlink — even pointing at a file that
        // carries our marker — must not be followed or replaced.
        let target = dir.join("real-target");
        fs::write(&target, resolver_contents(&servers())).unwrap();
        let link = dir.join("corp.example.com");
        symlink(&target, &link).unwrap();

        let err = apply_to_dir(&dir, &servers(), &["corp.example.com".to_string()]).unwrap_err();
        assert!(matches!(err, PlatformError::CommandFailed(_)));
        // The symlink is left as-is (not followed, not replaced by a file).
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_domain_is_rejected_before_any_write() {
        let dir = temp_dir("invalid-domain");
        let err = apply_to_dir(&dir, &servers(), &["../escape".to_string()]).unwrap_err();
        assert!(matches!(err, PlatformError::CommandFailed(_)));
        assert!(!dir.join("../escape").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_prune_rolls_back_this_calls_writes() {
        use std::process::Command;
        // Exercises the prune-failure branch the write-failure tests can't reach:
        // a stale managed file for a dropped domain is made immutable (chflags
        // uchg) so remove_managed fails AFTER the fresh write for a.com succeeds.
        let dir = temp_dir("prune-rollback");
        let stale = dir.join("b.com");
        fs::write(&stale, resolver_contents(&servers())).unwrap();
        assert!(Command::new("chflags")
            .args(["uchg"])
            .arg(&stale)
            .status()
            .unwrap()
            .success());

        let err = apply_to_dir(&dir, &servers(), &["a.com".to_string()]).unwrap_err();
        assert!(matches!(err, PlatformError::CommandFailed(_)));
        // The freshly-written a.com (Prior::Absent) is rolled back, and the
        // un-prunable file is left untouched.
        assert!(
            !dir.join("a.com").exists(),
            "the fresh write must be rolled back when prune fails"
        );
        assert!(stale.exists(), "an un-prunable file is left intact");

        // Teardown: clear the immutable flag so the dir can be removed.
        let _ = Command::new("chflags")
            .args(["nouchg"])
            .arg(&stale)
            .status();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_managed_state_reconstructs_domains_and_servers() {
        let dir = temp_dir("read-managed");
        apply_to_dir(
            &dir,
            &servers(),
            &["a.example.com".to_string(), "b.example.com".to_string()],
        )
        .unwrap();

        let state = read_managed_state(&dir);
        // Domains are the managed filenames (sorted order is stable).
        assert_eq!(
            state.routing_domains,
            vec!["a.example.com", "b.example.com"]
        );
        // Servers are de-duplicated across the per-domain files.
        assert_eq!(state.servers, vec!["10.0.0.1", "10.0.0.2"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_managed_state_ignores_unmanaged_files() {
        let dir = temp_dir("read-unmanaged");
        apply_to_dir(&dir, &servers(), &["a.example.com".to_string()]).unwrap();
        // A resolver file the user wrote by hand must not appear in the read-back.
        fs::write(dir.join("user.example"), "nameserver 198.51.100.9\n").unwrap();

        let state = read_managed_state(&dir);
        assert_eq!(state.routing_domains, vec!["a.example.com"]);
        assert_eq!(state.servers, vec!["10.0.0.1", "10.0.0.2"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_managed_state_on_missing_dir_is_empty() {
        let mut missing = std::env::temp_dir();
        missing.push(format!("splitway-absent-readback-{}", std::process::id()));
        let _ = fs::remove_dir_all(&missing);
        let state = read_managed_state(&missing);
        assert!(state.servers.is_empty());
        assert!(state.routing_domains.is_empty());
    }

    #[test]
    fn read_managed_state_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("read-symlink");
        // A genuine managed file that must appear in the read-back.
        apply_to_dir(&dir, &servers(), &["a.example.com".to_string()]).unwrap();
        // A managed-marker target *outside* the scanned dir, and a symlink inside
        // the dir whose name looks like a routing domain pointing at it.
        // `symlink_metadata` must not follow the link, so neither the symlink's
        // name nor the target's servers leak into the live state.
        let outside = temp_dir("read-symlink-target");
        let target = outside.join("target");
        fs::write(&target, resolver_contents(&["198.51.100.9".to_string()])).unwrap();
        symlink(&target, dir.join("evil.example.com")).unwrap();

        let state = read_managed_state(&dir);
        // Only the real managed file's domain; the symlink's name is excluded.
        assert_eq!(state.routing_domains, vec!["a.example.com"]);
        // The symlink target's server (198.51.100.9) is not read through.
        assert_eq!(state.servers, vec!["10.0.0.1", "10.0.0.2"]);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }
}
