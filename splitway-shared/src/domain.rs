//! Domain input normalization and suffix-aware coverage matching, shared by the
//! daemon and every client. The daemon is the authoritative normalizer (it
//! normalizes on `add_domain` and `CheckDomain`); clients may use these to
//! pre-validate input.
//!
//! Boundary: this module is about *names*. Splitway governs DNS (which resolver
//! answers a name), not IP routing (whether the resolved address is reachable
//! through the tunnel) — see `docs/architecture.md`.

use thiserror::Error;

/// Why an input could not be reduced to a bare host.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    /// The input was empty or whitespace-only.
    #[error("empty host")]
    Empty,
    /// The input is not a bare host (e.g. a path was pasted without a scheme, or
    /// a bracketed IPv6 literal, which is not a routing domain).
    #[error("not a bare host: {0}")]
    NotAHost(String),
    /// The extracted host has an empty label (a leading/trailing/doubled dot) or
    /// embedded whitespace.
    #[error("invalid host: {0}")]
    InvalidHost(String),
}

/// Fold a host for case-insensitive, trailing-dot-insensitive comparison:
/// trim, strip a *single* trailing dot (the valid FQDN root form), lowercase
/// ASCII. Stripping only one dot leaves a doubled trailing dot (`example.com..`)
/// as an empty label that [`validate_host`] then rejects, rather than silently
/// folding to the parent. `to_ascii_lowercase` deliberately leaves non-ASCII
/// bytes untouched — see the IDN note on [`normalize_host`].
fn fold(host: &str) -> String {
    let host = host.trim();
    let host = host.strip_suffix('.').unwrap_or(host);
    host.to_ascii_lowercase()
}

/// Normalize a pasted URL or a bare host into a bare, lowercased host.
///
/// Accepts `https://vault.sub.example.com/x?y=1` (→ `vault.sub.example.com`),
/// `sub.example.com:443` (→ `sub.example.com`), `user@host`, and a trailing
/// dot. Rejects empty/whitespace-only input, a path pasted without a scheme
/// (a bare `host/x`), IP literals (bare/bracketed IPv6 and IPv4 — a routing
/// domain is a name, not an address), and hosts with empty labels.
///
/// TODO(idn): IDN / punycode is out of scope. Non-ASCII hosts pass through as-is
/// (only ASCII is lowercased); a future change should punycode-encode them so an
/// IDN and its A-label compare equal.
pub fn normalize_host(input: &str) -> Result<String, DomainError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DomainError::Empty);
    }

    // Cut a query/fragment from the whole input first — they can follow either a
    // URL or a bare `host[:port]` — then strip the scheme and path. A non-URL (or
    // an empty scheme like `://x`) is treated as the authority directly; the slash
    // check below then rejects a bare host that carried a path.
    let cut = trimmed.split(['?', '#']).next().unwrap_or("");
    let authority = match cut.split_once("://") {
        Some((scheme, rest)) if !scheme.is_empty() => rest.split('/').next().unwrap_or(""),
        _ => cut,
    };

    // Without a scheme, a slash means a path was pasted onto a bare host (or the
    // scheme was empty, e.g. `://x`); reject rather than guess where the host ends.
    if authority.contains('/') {
        return Err(DomainError::NotAHost(trimmed.to_string()));
    }

    // Strip optional `userinfo@`.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);

    // An IPv6 literal — bracketed (`[::1]`) or bare (`2001:db8::1`) — is not a
    // routing domain. A domain has at most one colon (the `host:port` separator),
    // so a bracket, or two-or-more colons, means an address (or a zone-id form).
    if host_port.starts_with('[') || host_port.matches(':').count() >= 2 {
        return Err(DomainError::NotAHost(trimmed.to_string()));
    }

    // Strip a `:port` suffix. A domain has no colons, so the host is everything
    // before the (single) colon.
    let host = host_port.split(':').next().unwrap_or(host_port);

    let host = fold(host);

    // An IPv4 literal is not a routing domain either — like the IPv6 cases above,
    // it is an address, not a name (it would never match a real DNS lookup, and a
    // route-check on it is meaningless). Reject it for symmetry.
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(DomainError::NotAHost(trimmed.to_string()));
    }

    validate_host(&host).map(|()| host)
}

/// Reject an empty host, embedded whitespace, an empty label (a
/// leading/trailing/doubled dot), or a label that begins or ends with a hyphen.
/// The hyphen rule matters at the trust boundary: a leading-`-` label is never a
/// valid RFC 1035/1123 hostname and is the textbook argument-injection shape
/// (the host flows verbatim into `resolvectl`'s argv), so the normalizer — the
/// gate for untrusted IPC input — must reject it. Interior hyphens and non-ASCII
/// label bytes (IDN, a pass-through TODO) stay lenient.
fn validate_host(host: &str) -> Result<(), DomainError> {
    if host.is_empty() {
        return Err(DomainError::Empty);
    }
    if host.contains(char::is_whitespace) {
        return Err(DomainError::InvalidHost(host.to_string()));
    }
    let label_ok =
        |label: &str| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-');
    if !host.split('.').all(label_ok) {
        return Err(DomainError::InvalidHost(host.to_string()));
    }
    Ok(())
}

/// Whether two hosts are the same name, ignoring case and a trailing dot — the
/// equivalence used for add/remove dedup so `Example.com`, `example.com.`, and
/// `example.com` are one domain. Pure, no I/O.
pub fn same_host(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}

/// Whether the configured routing `domain` covers `host` (suffix-aware):
/// systemd-resolved routes a domain *and all its subdomains*, so
/// `sub.example.com` is covered by a configured `example.com`. Both sides are
/// folded (lowercase, trailing dot stripped) so a pre-existing un-normalized
/// config entry still matches. Pure, no I/O.
pub fn domain_covers(domain: &str, host: &str) -> bool {
    let domain = fold(domain);
    let host = fold(host);
    if domain.is_empty() || host.is_empty() {
        return false;
    }
    host == domain || host.ends_with(&format!(".{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls_ports_case_and_trailing_dot() {
        let cases = [
            ("example.com", "example.com"),
            ("EXAMPLE.com", "example.com"),
            ("example.com.", "example.com"),
            ("  sub.example.com  ", "sub.example.com"),
            (
                "https://vault.sub.example.com/x?y=1",
                "vault.sub.example.com",
            ),
            ("http://example.com", "example.com"),
            ("https://example.com:8443/path", "example.com"),
            ("example.com:443", "example.com"),
            ("user@sub.example.com", "sub.example.com"),
            ("https://user:pass@sub.example.com:443/p", "sub.example.com"),
            ("localhost", "localhost"),
            // Interior hyphens are fine; punycode (xn--) too (leading char is `x`).
            ("my-host.example.com", "my-host.example.com"),
            ("xn--caf-dma.example.com", "xn--caf-dma.example.com"),
            // A scheme we do not special-case still yields the authority host.
            ("ftp://files.example.org/pub", "files.example.org"),
            // Query / fragment are stripped even without a scheme or path.
            ("example.com?q=1", "example.com"),
            ("example.com#frag", "example.com"),
            ("example.com:443?q=1", "example.com"),
        ];
        for (input, want) in cases {
            assert_eq!(
                normalize_host(input).as_deref(),
                Ok(want),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_input() {
        assert_eq!(normalize_host(""), Err(DomainError::Empty));
        assert_eq!(normalize_host("   "), Err(DomainError::Empty));
        // A path pasted onto a bare host (no scheme) is ambiguous → rejected.
        assert!(matches!(
            normalize_host("example.com/path"),
            Err(DomainError::NotAHost(_))
        ));
        // Bracketed IPv6 literal is not a routing domain.
        assert!(matches!(
            normalize_host("[2001:db8::1]:443"),
            Err(DomainError::NotAHost(_))
        ));
        // A bare IPv6 literal is rejected too (not silently truncated at the
        // first colon).
        assert!(matches!(
            normalize_host("2001:db8::1"),
            Err(DomainError::NotAHost(_))
        ));
        assert!(matches!(
            normalize_host("2001:db8::"),
            Err(DomainError::NotAHost(_))
        ));
        // An empty scheme is not a valid URL — falls through and is rejected.
        assert!(matches!(
            normalize_host("://x"),
            Err(DomainError::NotAHost(_))
        ));
        // IPv4 literals are not routing domains (symmetry with the IPv6 cases).
        assert!(matches!(
            normalize_host("192.0.2.1"),
            Err(DomainError::NotAHost(_))
        ));
        assert!(matches!(
            normalize_host("https://192.0.2.1/"),
            Err(DomainError::NotAHost(_))
        ));
        // Empty labels.
        assert!(matches!(
            normalize_host(".leading.dot"),
            Err(DomainError::InvalidHost(_))
        ));
        assert!(matches!(
            normalize_host("double..dot"),
            Err(DomainError::InvalidHost(_))
        ));
        // Whitespace inside the host.
        assert!(matches!(
            normalize_host("has space.com"),
            Err(DomainError::InvalidHost(_))
        ));
        // A doubled trailing dot leaves an empty label and is rejected (only a
        // single trailing dot is the valid FQDN root form).
        assert!(matches!(
            normalize_host("example.com.."),
            Err(DomainError::InvalidHost(_))
        ));
        // A label may not begin or end with a hyphen (RFC LDH; also the
        // argument-injection shape that reaches resolvectl's argv).
        assert!(matches!(
            normalize_host("-x"),
            Err(DomainError::InvalidHost(_))
        ));
        assert!(matches!(
            normalize_host("--foo.example.com"),
            Err(DomainError::InvalidHost(_))
        ));
        assert!(matches!(
            normalize_host("foo-.example.com"),
            Err(DomainError::InvalidHost(_))
        ));
    }

    #[test]
    fn coverage_is_suffix_aware_and_normalized() {
        // Exact match.
        assert!(domain_covers("example.com", "example.com"));
        // Subdomain is covered.
        assert!(domain_covers("example.com", "vault.sub.example.com"));
        assert!(domain_covers("sub.example.com", "vault.sub.example.com"));
        // Case / trailing dot on either side still match.
        assert!(domain_covers("Example.COM", "VAULT.example.com."));
        // A sibling/parent is not covered.
        assert!(!domain_covers("sub.example.com", "example.com"));
        // A suffix that is not a label boundary must not match.
        assert!(!domain_covers("example.com", "notexample.com"));
        assert!(!domain_covers("example.com", "example.com.evil.test"));
        // Empty domain never covers.
        assert!(!domain_covers("", "example.com"));
    }

    #[test]
    fn same_host_ignores_case_and_trailing_dot() {
        assert!(same_host("Example.com", "example.com"));
        assert!(same_host("example.com.", "example.com"));
        assert!(same_host("EXAMPLE.COM.", "example.com"));
        // Different hosts (subdomain, sibling) are not the same.
        assert!(!same_host("a.example.com", "example.com"));
        assert!(!same_host("example.org", "example.com"));
    }
}
