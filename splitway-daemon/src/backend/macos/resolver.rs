//! Pure helpers for the `/etc/resolver` split-DNS files — no I/O, unit tested.
//!
//! Ownership is encoded in the file itself: every resolver file Splitway writes
//! starts with [`MANAGED_MARKER`]. Revert and reconcile only ever touch files
//! that carry it, so a resolver file the user wrote by hand is never removed.

use std::path::{Path, PathBuf};

/// First line of every resolver file Splitway creates. Used as the ownership
/// test (see [`is_managed`]); changing it would orphan files written by an
/// older daemon, so keep it stable.
pub(super) const MANAGED_MARKER: &str =
    "# Managed by splitway - do not edit; removed automatically when the VPN drops.";

/// The `/etc/resolver/<domain>` path for `domain` under `dir`.
pub(super) fn resolver_path(dir: &Path, domain: &str) -> PathBuf {
    dir.join(domain)
}

/// Build a resolver file body for `servers`: the ownership marker followed by
/// one `nameserver <ip>` line per server (`man 5 resolver`).
pub(super) fn resolver_contents(servers: &[String]) -> String {
    let mut out = String::with_capacity(MANAGED_MARKER.len() + 1 + servers.len() * 24);
    out.push_str(MANAGED_MARKER);
    out.push('\n');
    for server in servers {
        out.push_str("nameserver ");
        out.push_str(server);
        out.push('\n');
    }
    out
}

/// Was this file written by Splitway? True iff its first line is the marker.
pub(super) fn is_managed(contents: &str) -> bool {
    contents.lines().next() == Some(MANAGED_MARKER)
}

/// Is `domain` safe to use as a single `/etc/resolver` filename? Rejects names
/// that would escape the directory (`/`, `\`, `.`, `..`) or break the
/// single-component matching that prune relies on, plus control characters.
pub(super) fn is_valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain != "."
        && domain != ".."
        && !domain.contains('/')
        && !domain.contains('\\')
        && !domain.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_have_marker_then_nameservers() {
        let body = resolver_contents(&["10.0.0.1".to_string(), "10.0.0.2".to_string()]);
        assert_eq!(
            body,
            format!("{MANAGED_MARKER}\nnameserver 10.0.0.1\nnameserver 10.0.0.2\n")
        );
    }

    #[test]
    fn contents_with_no_servers_are_just_the_marker() {
        assert_eq!(resolver_contents(&[]), format!("{MANAGED_MARKER}\n"));
    }

    #[test]
    fn is_managed_only_for_our_marker() {
        assert!(is_managed(&resolver_contents(&["1.1.1.1".to_string()])));
        assert!(is_managed(MANAGED_MARKER));
        // A file the user wrote by hand must not be claimed.
        assert!(!is_managed("nameserver 1.1.1.1\n"));
        assert!(!is_managed("# some other tool\nnameserver 1.1.1.1\n"));
        assert!(!is_managed(""));
        // The marker must be the *first* line, not buried later.
        assert!(!is_managed(&format!(
            "nameserver 1.1.1.1\n{MANAGED_MARKER}\n"
        )));
    }

    #[test]
    fn valid_domain_accepts_hostnames_rejects_traversal() {
        assert!(is_valid_domain("corp.example.com"));
        assert!(is_valid_domain("internal"));
        // Path-traversal / escape attempts and control chars are rejected.
        assert!(!is_valid_domain(""));
        assert!(!is_valid_domain("."));
        assert!(!is_valid_domain(".."));
        assert!(!is_valid_domain("a/b"));
        assert!(!is_valid_domain("../etc/passwd"));
        assert!(!is_valid_domain("a\\b"));
        assert!(!is_valid_domain("a\nb"));
        assert!(!is_valid_domain("a\0b"));
    }

    #[test]
    fn resolver_path_joins_domain() {
        assert_eq!(
            resolver_path(Path::new("/etc/resolver"), "corp.example.com"),
            PathBuf::from("/etc/resolver/corp.example.com")
        );
    }
}
