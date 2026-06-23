//! IPC protocol shared by `splitway-daemon` and `splitway-cli` — the single
//! source of truth for the wire format so both sides cannot drift apart.
//!
//! Transport: a Unix domain socket. Wire format: newline-delimited JSON —
//! one [`RequestEnvelope`] object per line, to which the daemon replies with
//! exactly one [`Response`] line.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::VpnBackend;
use crate::domain;

/// Bumped on any incompatible change to [`Request`] / [`Response`]. The
/// daemon rejects envelopes whose version it does not speak.
///
/// Bumped to `2` in Phase 4 for the additive `GetConfig`/`SetConfig` verbs and
/// the [`Response::Config`] reply. Bumped to `3` in Phase 5 for the additive
/// `ListInterfaces` verb / [`Response::Interfaces`] reply and the richer
/// [`StatusInfo`] (the `applied` mapping, [`RoutingState`] and
/// [`DetectorHealth`] — all additive). Bumped to `4` in Phase 5c for the
/// additive [`RoutingState::ConfigInvalid`] variant (the malformed-config freeze
/// surfaced over IPC). Bumped to `5` in Phase 5b for the additive
/// `CheckDomain` verb / [`Response::DomainCheck`] reply (the domain route-check).
/// Bumped to `6` in Phase 5d for the additive `Verify` verb /
/// [`Response::Verify`] reply (live DNS read-back + drift detection — `reality`
/// alongside `status`'s `belief`). Bumped to `7` in Phase 7d for the additive
/// [`StatusInfo::detected_dns`] field — the DNS server(s) the configured VPN
/// interface is currently *detected* to expose, surfaced independently of whether
/// routing is applied so a client can show the interface's resolver read-only
/// (the interface-centric, DNS-auto model the Tauri GUI renders). Bumped to `8`
/// for the additive [`LinkDnsState::default_route`] field (the per-link
/// DNS-default-route / catch-all flag, read back so a full-tunnel link that
/// resolves *every* name — leaking past a narrow split — is detected) and the
/// matching additive `default_route_leak` field on [`DriftVerdict::Drifted`]. The daemon
/// enforces *strict equality* (see
/// `daemon::ipc::process_line`): a daemon rejects a client whose version differs,
/// and vice versa, so there is no silent mixed-version operation. The daemon, CLI
/// and GUI all build from this one workspace, so they upgrade in lockstep; a
/// mismatch only happens across separately-updated installs and is surfaced as
/// actionable "update splitway" guidance, never a raw decode error.
pub const PROTOCOL_VERSION: u32 = 8;

/// Stable prefix the daemon uses to introduce a protocol-version-mismatch
/// error reply. Shared so a client (CLI/GUI) can recognize skew and render
/// "update splitway" guidance distinctly, instead of string-matching a literal
/// that could drift from the daemon's wording.
pub const VERSION_MISMATCH_PREFIX: &str = "protocol version mismatch";

/// Versioned wrapper around a [`Request`]. Carrying the version per request
/// keeps a single-shot client trivial — no separate handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub version: u32,
    pub request: Request,
}

impl RequestEnvelope {
    pub fn new(request: Request) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Current daemon + DNS routing state.
    Status,
    /// Enable rule application (persisted).
    Enable,
    /// Disable rule application and revert (persisted).
    Disable,
    /// Add a domain to route through the VPN (persisted).
    AddDomain(String),
    /// Remove a domain (persisted). Absent domain is a no-op success.
    RemoveDomain(String),
    /// List the configured domains.
    ListDomains,
    /// Re-read the config file from disk and reconcile.
    ReloadConfig,
    /// Read the editable config projection (the settings not covered by the
    /// other verbs). Replied to with [`Response::Config`].
    GetConfig,
    /// Update the editable config projection (persisted). The daemon handles
    /// this in its single-writer state actor via the same `commit()` path as
    /// the other mutating verbs, so it stays the sole config writer.
    SetConfig(ConfigView),
    /// Enumerate the host's network interfaces so a client can offer an
    /// interface picker without itself touching the platform or holding
    /// privileges. Read-only; replied to with [`Response::Interfaces`].
    ListInterfaces,
    /// Check whether a domain routes through the VPN. The argument is raw input —
    /// a pasted URL or a bare host — which the daemon normalizes. Read-only;
    /// replied to with [`Response::DomainCheck`]. Reports coverage and which
    /// resolver answered, *not* reachability (Splitway governs DNS, not IP
    /// routing — see `docs/architecture.md`).
    CheckDomain(String),
    /// Read the **live** per-link DNS state back from the system and report any
    /// drift from what the daemon believes it installed (`applied`). Read-only;
    /// replied to with [`Response::Verify`]. This is the explicit `reality` check
    /// — distinct from [`Request::Status`], which stays cheap `belief` (it never
    /// reads the system back). Reports the live resolver state vs the believed
    /// one, *not* reachability (Splitway governs DNS, not IP routing — see
    /// `docs/architecture.md`).
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// The request succeeded and carries no data.
    Ok,
    /// Reply to [`Request::Status`].
    Status(StatusInfo),
    /// Reply to [`Request::ListDomains`].
    Domains(Vec<String>),
    /// Reply to [`Request::GetConfig`].
    Config(ConfigView),
    /// Reply to [`Request::ListInterfaces`].
    Interfaces(Vec<InterfaceInfo>),
    /// Reply to [`Request::CheckDomain`].
    DomainCheck(DomainCheckInfo),
    /// Reply to [`Request::Verify`].
    Verify(VerifyInfo),
    /// The request failed; the string is a human-readable reason.
    Error(String),
}

/// The editable projection of `LocalConfig` carried over IPC — deliberately a
/// small, dedicated wire type rather than `LocalConfig` itself, so the wire
/// format stays independently versionable and is not coupled to the on-disk
/// serde layout.
///
/// It covers exactly the gap the other verbs leave: `vpn_name`, `vpn_backend`
/// and the `openvpn.*` settings. `enabled` stays owned by `Enable`/`Disable`
/// and the domain list by `AddDomain`/`RemoveDomain`/`ListDomains`, so
/// [`Request::SetConfig`] is a *partial* update that never clobbers another
/// verb's slice of the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigView {
    /// The configured VPN interface (device) name.
    pub vpn_name: String,
    /// Which Linux VPN detector to use. `#[serde(default)]` keeps the wire type
    /// forward-compatible if the field is ever omitted by an older peer.
    #[serde(default)]
    pub vpn_backend: VpnBackend,
    /// Standalone-OpenVPN management endpoint (`host:port` or a unix socket
    /// path); ignored unless `vpn_backend = openvpn`.
    #[serde(default)]
    pub openvpn_management: String,
    /// Optional path to the management password file. `None` = no password.
    #[serde(default)]
    pub openvpn_management_password_file: Option<String>,
    /// Read-only: the daemon's effective config file path. The daemon fills
    /// this on [`Request::GetConfig`] and *ignores* it on
    /// [`Request::SetConfig`] — the active path is fixed at daemon launch
    /// (via `--config`), so the GUI cannot switch it at runtime.
    #[serde(default)]
    pub config_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    /// Whether rule application is enabled.
    pub enabled: bool,
    /// The *configured* VPN interface name (`vpn_name`), not necessarily the one
    /// rules are applied to right now — see [`AppliedInfo::interface`]. Kept
    /// named `interface` for wire continuity; it is the configured device name.
    pub interface: String,
    /// Whether the VPN interface is currently up.
    pub vpn_up: bool,
    /// The DNS mapping currently applied to the system, or `None` when nothing
    /// is applied. `is_some()` recovers the old boolean "applied?" meaning while
    /// also answering "which domains route through which DNS, on which link".
    pub applied: Option<AppliedInfo>,
    /// A self-explaining summary of why routing is (or is not) active right now,
    /// mapped from the daemon's own reconcile decision — see [`RoutingState`].
    pub routing_state: RoutingState,
    /// The DNS server(s) the configured VPN interface is currently *detected* to
    /// expose, surfaced independently of whether rules are applied. This is the
    /// last detector reading for `interface` (the `vpn_name`): non-empty whenever
    /// the configured interface is up and pushing DNS, even while routing is
    /// disabled or no domains are configured (cases where [`applied`] is `None`).
    /// Empty when no interface is configured, the interface is down, or it pushes
    /// no DNS (the [`RoutingState::NoDnsFromVpn`] case). Lets a client show the
    /// interface's resolver read-only — the interface-centric, DNS-auto model —
    /// without inferring it from [`applied`] (which only exists once applied).
    ///
    /// [`applied`]: StatusInfo::applied
    pub detected_dns: Vec<String>,
    /// Whether the VPN-detector watch is running, idle, or failed to start.
    pub detector_health: DetectorHealth,
    /// The configured domains.
    pub domains: Vec<String>,
}

/// A snapshot of the DNS rules currently applied to the system — the wire
/// projection of the daemon's internal applied state. Carried in
/// [`StatusInfo::applied`] so a client can *verify* what the daemon believes it
/// has installed (which domains route through which DNS, on which interface),
/// not just that *something* is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedInfo {
    /// The interface the rules are applied to (may differ from the configured
    /// [`StatusInfo::interface`] during a live interface switch).
    pub interface: String,
    /// The domains routed through the VPN DNS.
    pub domains: Vec<String>,
    /// The VPN DNS servers the domains are routed to.
    pub dns_servers: Vec<String>,
}

/// The result of a [`Request::CheckDomain`] route-check: whether a configured
/// routing domain covers the host, plus the live-resolution result and enough
/// context for a client to phrase the verdict without over-claiming. Reports
/// coverage and which resolver answered — **not** reachability (Splitway governs
/// DNS, not IP routing; see `docs/architecture.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainCheckInfo {
    /// The normalized host that was checked (a pasted URL is reduced to its host).
    pub host: String,
    /// Whether a configured routing domain covers `host` (suffix-aware).
    pub covered: bool,
    /// The configured domain that covers `host`, when `covered`.
    pub matched_domain: Option<String>,
    /// The configured VPN interface (`vpn_name`), so a client can tell whether
    /// the link that answered is the VPN's. Empty when no interface is configured.
    pub vpn_interface: String,
    /// The live-resolution result, or `None` when resolution was not attempted,
    /// failed, or is unsupported on this platform (never an `Error` reply).
    pub resolution: Option<ResolutionInfo>,
    /// Whether rule application is enabled — context so a client can say
    /// "covered, but disabled" rather than implying it routes right now.
    pub enabled: bool,
    /// Whether the configured VPN interface is currently up — context so a client
    /// can say "covered, but the VPN is down" rather than implying live routing.
    pub vpn_up: bool,
    /// The daemon's overall routing state (its *belief*: whether usable split-DNS
    /// rules are actually installed). A client must key the "routed right now"
    /// verdict on this — `enabled && vpn_up` is not enough, since the VPN can be
    /// up while pushing no DNS (`NoDnsFromVpn`) or an apply can have failed
    /// (`ApplyFailed`), in which case nothing is routed.
    pub routing_state: RoutingState,
}

/// The live-resolution result for one host. `via_interface` / `via_dns` are
/// best-effort attribution: Linux (systemd-resolved) reports the answering link;
/// the resolver IP is usually absent from `resolvectl query`, and macOS cannot
/// attribute either — so both are `Option`. Addresses are the resolved IP(s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionInfo {
    /// The resolved IP address(es), as text.
    pub addresses: Vec<String>,
    /// The link that answered, when the platform can attribute it (Linux-strong;
    /// typically `None` on macOS).
    pub via_interface: Option<String>,
    /// The resolver that answered, when known (often `None` — `resolvectl query`
    /// does not report it).
    pub via_dns: Option<String>,
}

/// The **live** per-link DNS state read back from the system — the `reality`
/// counterpart to the daemon's `belief` ([`AppliedInfo`]). On Linux it is parsed
/// from `resolvectl status <iface>`; on macOS it is reconstructed best-effort from
/// the managed `/etc/resolver` files Splitway wrote (which are keyed by domain,
/// not interface). An all-empty value means the read-back was unavailable
/// (unsupported platform, a vanished link, or a transient command failure) or the
/// link genuinely carries no Splitway DNS — the two are indistinguishable here, by
/// design: a read-back failure degrades to empty rather than an error.
///
/// `routing_domains` are stored **without** any leading `~`: `resolvectl` prints a
/// routing-only domain as `~example.com`, but the wire form is the plain domain so
/// a bare-vs-`~` difference is never read as drift (see [`compare_drift`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkDnsState {
    /// The DNS servers the link currently resolves through (the union of
    /// `resolvectl`'s `Current DNS Server` and `DNS Servers`), as text.
    pub servers: Vec<String>,
    /// The routing/search domains the link currently routes, each with any
    /// leading `~` already stripped to the plain domain.
    pub routing_domains: Vec<String>,
    /// Whether the link is systemd-resolved's DNS **default route** (catch-all):
    /// `Some(true)` means it resolves *every* name that matches no other link's
    /// routing domain — an implicit `~.` that defeats a narrow split (a full-tunnel
    /// VPN gets this set automatically because it carries the default IP route);
    /// `Some(false)` means it resolves only its `routing_domains`; `None` means the
    /// flag is unknown or not applicable — the read-back did not learn it (an older
    /// peer, or a transient failure) or the platform has no link-level catch-all
    /// concept (macOS `/etc/resolver` files are per-domain). `None` must never be
    /// treated as drift — see [`compare_drift`]. Parsed from `resolvectl`'s
    /// `Default Route:` line.
    #[serde(default)]
    pub default_route: Option<bool>,
}

/// The result of a [`Request::Verify`] read-back: the live link state plus the
/// drift verdict comparing it against what the daemon believes it installed. This
/// is observability only — the daemon does **not** auto-remediate on drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyInfo {
    /// The live per-link DNS state read back from the system (empty when the
    /// read-back was unavailable — see [`LinkDnsState`]).
    pub live: LinkDnsState,
    /// How the live state compares to the daemon's `applied` belief.
    pub drift: DriftVerdict,
}

/// Whether the live DNS state matches what the daemon believes it installed.
/// Produced by [`compare_drift`]; report-only (no auto-remediation this phase).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriftVerdict {
    /// The daemon believes nothing is installed (`applied` is `None`), so there is
    /// nothing to compare the live state against.
    NotApplicable,
    /// Every believed DNS server is present live and every believed domain is
    /// covered by a live routing domain — reality matches belief.
    InSync,
    /// The live state diverges from belief. Each field names exactly what is
    /// missing so a client can render the specifics. A `Drifted` is produced when
    /// *any* of the three fields is non-empty/true; an all-clear (no missing
    /// servers, no unrouted domains, no leak) is [`DriftVerdict::InSync`] instead.
    Drifted {
        /// Believed DNS servers not found in the live `servers`.
        missing_servers: Vec<String>,
        /// Believed domains not covered by any live routing domain.
        unrouted_domains: Vec<String>,
        /// The live link is a **catch-all** while the believed split is narrow: it
        /// resolves *every* unmatched name through the VPN resolver, not just the
        /// configured domains, so a name the user never configured still leaks to
        /// the VPN. This is drift even when `missing_servers` and `unrouted_domains`
        /// are both empty (the narrow domains are technically present, but the link
        /// routes far more than them). Set when the live link is a catch-all by
        /// either signal — `default_route == Some(true)` or a live route-all (`~.`)
        /// routing domain — and the belief is not itself route-all. See
        /// [`compare_drift`].
        default_route_leak: bool,
    },
}

/// Compare a live [`LinkDnsState`] against what the daemon believes it installed
/// (`applied`) and report any [`DriftVerdict`]. Pure, no I/O.
///
/// - `applied` is `None` → [`DriftVerdict::NotApplicable`] (nothing to verify).
/// - Otherwise, normalized so a cosmetic difference is never false drift:
///   - every believed `dns_servers` entry must be **present** in the live
///     `servers`. Servers compare by canonical IP equality when both parse as IPs
///     (so `2001:DB8::1` equals `2001:db8::1` and a zero-compressed form equals
///     its expansion), falling back to a case-folded string match otherwise.
///   - every believed `domains` entry must be **covered** by some live
///     `routing_domain`, suffix-aware via [`domain::domain_covers`] (so a live
///     `example.com` covers a believed `sub.example.com`, and case / a trailing
///     dot / a stripped `~` never matter).
///   - the live link must **not** be a catch-all while the belief is a narrow
///     split. A link is a catch-all if `default_route == Some(true)` OR it carries
///     a live route-all routing domain (`~.`, parsed to `.`) — either makes it
///     resolve every unmatched name through the VPN, leaking names the user never
///     configured. That is drift even when the narrow domains are all present. A
///     `None` `default_route` with no route-all domain (read-back did not learn the
///     flag, or macOS) is never a leak, and a belief that is *itself* route-all (a
///     deliberate full-tunnel split) is not a leak either.
///   - all present, covered, and no leak → [`DriftVerdict::InSync`]; otherwise
///     [`DriftVerdict::Drifted`] naming the missing servers, unrouted domains, and
///     whether the link leaks as a catch-all.
pub fn compare_drift(live: &LinkDnsState, applied: Option<&AppliedInfo>) -> DriftVerdict {
    let Some(applied) = applied else {
        return DriftVerdict::NotApplicable;
    };

    let missing_servers: Vec<String> = applied
        .dns_servers
        .iter()
        .filter(|believed| {
            !live
                .servers
                .iter()
                .any(|live| server_matches(live, believed))
        })
        .cloned()
        .collect();

    // A believed domain is routed live if any live routing domain covers it
    // (suffix-aware): the live link may route a broader parent (`example.com`)
    // that still covers the believed `sub.example.com`, or route *everything* via
    // the systemd route-all marker (`~.`, parsed to the root `.`).
    let unrouted_domains: Vec<String> = applied
        .domains
        .iter()
        .filter(|believed| {
            // Strip a leading `~` (systemd's route-only marker) from the believed
            // domain too — the live parser already strips it, so a configured /
            // hand-edited `~corp.example.com` must compare against the live
            // `corp.example.com` rather than read as drift. Symmetric with the
            // parser's normalization.
            let believed = believed.as_str();
            let believed = believed.strip_prefix('~').unwrap_or(believed);
            !live
                .routing_domains
                .iter()
                .any(|live| is_route_all(live) || domain::domain_covers(live, believed))
        })
        .cloned()
        .collect();

    // The live link is the DNS default route (catch-all): it resolves every name
    // that matches no routing domain through the VPN resolver, so a name the user
    // never configured (a sibling of a configured domain, or anything else) still
    // leaks to the VPN. This defeats a narrow split even though the configured
    // domains are technically present, so it is drift in its own right. Gated to
    // `Some(true)`: a `None` flag (read-back did not learn it, or a platform with
    // no link-level catch-all) must never fabricate drift. A belief that is itself
    // route-all is a deliberate full-tunnel split, where catch-all is correct, not
    // a leak.
    let believes_route_all = applied.domains.iter().any(|believed| {
        let believed = believed.trim();
        let believed = believed.strip_prefix('~').unwrap_or(believed);
        is_route_all(believed)
    });
    // The live link is a catch-all by EITHER signal: systemd-resolved flags it the
    // DNS default route (`default_route == Some(true)`), OR a VPN manager installed
    // the catch-all as a live route-all routing domain (`~.`, parsed to `.`). The
    // two are equivalent ways to make a link answer every unmatched name, so both
    // must count — otherwise a manager that sets only `~.` (leaving the flag
    // `None`/`Some(false)`) would leak while the suffix-aware coverage loop above
    // reports every believed domain "covered" by `.`, hiding the leak as InSync.
    // Against a narrow belief either is a leak; a belief that is itself route-all
    // (a deliberate full-tunnel split) wants the catch-all, so it is not.
    let live_is_catch_all = live.default_route == Some(true)
        || live
            .routing_domains
            .iter()
            .any(|domain| is_route_all(domain));
    let default_route_leak = live_is_catch_all && !believes_route_all;

    if missing_servers.is_empty() && unrouted_domains.is_empty() && !default_route_leak {
        DriftVerdict::InSync
    } else {
        DriftVerdict::Drifted {
            missing_servers,
            unrouted_domains,
            default_route_leak,
        }
    }
}

/// Whether a live routing domain is systemd's route-all marker — the DNS root,
/// printed by `resolvectl` as `~.` and parsed to `.` (the `~` stripped). It
/// routes *every* query to the link, so it covers any believed domain. Without
/// this, a full-tunnel link that legitimately carries `~.` would be reported as
/// drift against every believed domain (`domain_covers(".", x)` folds `.` to the
/// empty domain and never matches).
fn is_route_all(domain: &str) -> bool {
    domain.trim() == "."
}

/// Extract the bare IP from a resolver token, dropping the optional decorations
/// systemd prints around a server: `ADDRESS[:PORT][%ifname]#SNI` (per `man
/// resolvectl`), with an IPv6 address bracketed when a port follows
/// (`[2001:db8::1]:53`). Returns `None` for a non-address token.
///
/// Shared by the `resolvectl status` parser (`linux/status.rs`) and the drift
/// comparison so both normalize a server **the same way**: otherwise a decoration
/// kept on one side — e.g. the detector's scoped `fe80::1%wg0` believed server vs
/// the bare `fe80::1` the live parser strips to — would read as false drift.
pub fn server_address(token: &str) -> Option<std::net::IpAddr> {
    use std::net::IpAddr;
    let token = token.trim();
    // Drop the `#SNI` (DNS-over-TLS server name) and a `%ifname` scope first.
    let token = token.split('#').next().unwrap_or(token);
    let token = token.split('%').next().unwrap_or(token);
    // A bracketed IPv6 literal, optionally with a trailing `:PORT`.
    let candidate = if let Some(rest) = token.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if token.parse::<IpAddr>().is_ok() {
        // A bare address — including an unbracketed IPv6, which has >= 2 colons
        // and so must be accepted before the `:PORT` strip below.
        token
    } else {
        // A bare `v4:port` has a single colon; strip the port. (A bare IPv6 was
        // already accepted above, so a remaining colon here is a port separator.)
        token.rsplit_once(':').map_or(token, |(host, _port)| host)
    };
    candidate.parse::<IpAddr>().ok()
}

/// Whether two DNS server strings denote the same resolver. Both are reduced to
/// their bare IP via [`server_address`] (so a `:port` / `%ifname` / `#SNI`
/// decoration on either side, and IPv6 case / zero-compression, never matter),
/// then compared as canonical addresses. A token that is not an address at all
/// falls back to a case-folded, trailing-dot-insensitive string match so it still
/// compares sanely rather than being treated as always-different.
fn server_matches(a: &str, b: &str) -> bool {
    match (server_address(a), server_address(b)) {
        (Some(a), Some(b)) => a == b,
        _ => domain::same_host(a, b),
    }
}

/// Why DNS routing is — or is not — active right now, mapped from the daemon's
/// reconcile decision. This is *belief*: what the daemon intends given its
/// config and the observed VPN state, not a read-back of the live system
/// (reality / drift detection is a later phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingState {
    /// Rule application is disabled (`enabled = false`).
    Disabled,
    /// No domains are configured, so there is nothing to route.
    NoDomains,
    /// The configured VPN interface is not up.
    VpnDown,
    /// The VPN is up but exposes no DNS servers to route the domains to.
    NoDnsFromVpn,
    /// Rules are applied — the daemon has installed its intended mapping. This
    /// is belief, not a read-back: it means the apply call succeeded, not that
    /// the live system has been re-verified (drift detection is a later phase).
    Applied,
    /// The last apply (or revert) failed, so the system may be out of sync and
    /// a re-apply is pending.
    ApplyFailed,
    /// The config file on disk does not parse (a malformed hand-edit). Routing
    /// reflects the last-good config the daemon froze on; this is the
    /// highest-precedence state, and it clears automatically once the file
    /// parses again. See the daemon's `on_config_changed`.
    ConfigInvalid,
}

/// Health of the VPN-detector watch, set by the daemon when it (re-)arms the
/// watch and reported in [`StatusInfo::detector_health`]. Lets a client say
/// "the watch is up / idle / failed to start" instead of inferring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectorHealth {
    /// A detector watch is running for the configured interface.
    Active,
    /// No watch is running because no interface is configured (`vpn_name` is
    /// empty). Auto-apply is intentionally off, not broken.
    Inactive,
    /// The watch failed to start (e.g. NetworkManager absent, or a bad OpenVPN
    /// management endpoint). Auto-apply is off; the string is the reason.
    Error(String),
}

/// One enumerated network interface, for the client's interface picker. A small
/// dedicated wire type (like [`ConfigView`]) so the picker need not touch the
/// platform or hold privileges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceInfo {
    /// The interface (device) name, e.g. `tun0`, `eth0`, `lo`.
    pub name: String,
    /// Whether the interface is currently up.
    pub up: bool,
    /// A name-prefix heuristic flag (`tun*` / `utun*` / `wg*` / `tap*` / `ppp*`
    /// / `gpd*`) hinting this is VPN-like. Advisory only — a client may use it
    /// to sort or highlight, never to filter the list.
    pub vpn_like: bool,
}

/// Concise human phrasing for [`StatusInfo::applied`], shared by the CLI and the
/// daemon's own `status` subcommand: `wg0 -> [a.com, b.com] via [10.0.0.1]`.
impl std::fmt::Display for AppliedInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -> [{}] via [{}]",
            self.interface,
            self.domains.join(", "),
            self.dns_servers.join(", ")
        )
    }
}

/// Human phrasing for [`StatusInfo::routing_state`], shared across clients.
impl std::fmt::Display for RoutingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            RoutingState::Disabled => "disabled",
            RoutingState::NoDomains => "no domains configured",
            RoutingState::VpnDown => "VPN down",
            RoutingState::NoDnsFromVpn => "VPN up, but it pushes no DNS",
            RoutingState::Applied => "applied",
            RoutingState::ApplyFailed => "apply failed (out of sync)",
            RoutingState::ConfigInvalid => "config file invalid (using last-good)",
        };
        f.write_str(text)
    }
}

/// Human phrasing for [`StatusInfo::detector_health`], shared across clients.
impl std::fmt::Display for DetectorHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectorHealth::Active => f.write_str("active"),
            DetectorHealth::Inactive => f.write_str("inactive (no interface configured)"),
            DetectorHealth::Error(reason) => write!(f, "error: {reason}"),
        }
    }
}

/// System-service socket directory, used when `XDG_RUNTIME_DIR` is unset (a
/// root service rather than a login session). macOS has no `/run` and a
/// read-only root volume, so the daemon (which creates this dir on bind) uses
/// `/var/run`; Linux keeps `/run` (systemd provisions `/run/splitway`).
#[cfg(target_os = "macos")]
const SYSTEM_SOCKET_DIR: &str = "/var/run/splitway";
#[cfg(not(target_os = "macos"))]
const SYSTEM_SOCKET_DIR: &str = "/run/splitway";

/// Resolve the control socket path: `$XDG_RUNTIME_DIR/splitway.sock` when
/// the runtime dir is set (already a `0700` user-private directory), else the
/// per-platform [`SYSTEM_SOCKET_DIR`] for a system service.
pub fn socket_path() -> PathBuf {
    socket_path_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Pure resolver split out so the `XDG_RUNTIME_DIR` preference is unit-testable
/// without mutating the process-global environment.
fn socket_path_from(runtime_dir: Option<OsString>) -> PathBuf {
    match runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("splitway.sock"),
        _ => PathBuf::from(SYSTEM_SOCKET_DIR).join("splitway.sock"),
    }
}

/// Synchronous single-shot client used by `splitway-cli` and the
/// `splitway-daemon status` subcommand: connect, send one request, read one
/// response. Single-shot, so a blocking `UnixStream` is simpler and needs no
/// async runtime.
#[cfg(unix)]
pub mod client {
    use super::{socket_path, Request, RequestEnvelope, Response};
    use std::fmt;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    #[derive(Debug)]
    pub enum ClientError {
        /// The socket could not be reached — most likely the daemon is down.
        NotRunning(std::io::Error),
        /// The socket exists but the caller is not allowed to connect — the
        /// daemon is running, but the user lacks access to its control socket.
        PermissionDenied(std::io::Error),
        /// An I/O error after connecting.
        Io(std::io::Error),
        /// A malformed or unexpected reply.
        Protocol(String),
    }

    impl fmt::Display for ClientError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                ClientError::NotRunning(e) => write!(
                    f,
                    "cannot reach the splitway daemon socket ({e}); is splitway-daemon running?"
                ),
                ClientError::PermissionDenied(e) => write!(
                    f,
                    "permission denied on the splitway daemon socket ({e}); \
                     the daemon is running but you lack access — try sudo or the daemon's group"
                ),
                ClientError::Io(e) => write!(f, "IPC I/O error: {e}"),
                ClientError::Protocol(m) => write!(f, "IPC protocol error: {m}"),
            }
        }
    }

    impl std::error::Error for ClientError {}

    /// Candidate sockets to try, in order: the per-user socket (if
    /// `$XDG_RUNTIME_DIR` is set) then the system socket. A login-session CLI
    /// thus reaches a system-service daemon (which binds [`SYSTEM_SOCKET_DIR`])
    /// even though its own `socket_path()` resolves to `$XDG_RUNTIME_DIR`.
    fn candidate_sockets() -> Vec<std::path::PathBuf> {
        let mut paths = vec![socket_path()];
        let system = std::path::PathBuf::from(super::SYSTEM_SOCKET_DIR).join("splitway.sock");
        if !paths.contains(&system) {
            paths.push(system);
        }
        paths
    }

    /// Connect to the daemon socket, send `request`, and return its reply.
    pub fn send_request(request: Request) -> Result<Response, ClientError> {
        let mut stream = None;
        let mut last_err = None;
        let mut permission_denied = None;
        for path in candidate_sockets() {
            match UnixStream::connect(&path) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                // A 0600 socket owned by another user (e.g. the root system
                // daemon) reports PermissionDenied — the daemon *is* running.
                // Keep it regardless of candidate order, so it is never masked
                // by a NotFound from a different candidate.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    permission_denied = Some(e);
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = match stream {
            Some(connected) => connected,
            None => {
                if let Some(e) = permission_denied {
                    return Err(ClientError::PermissionDenied(e));
                }
                let err = last_err.unwrap_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "no socket candidates")
                });
                return Err(ClientError::NotRunning(err));
            }
        };

        let mut writer = stream.try_clone().map_err(ClientError::Io)?;
        let mut line = serde_json::to_string(&RequestEnvelope::new(request))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        writer.write_all(line.as_bytes()).map_err(ClientError::Io)?;
        writer.flush().map_err(ClientError::Io)?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        if reader
            .read_line(&mut response_line)
            .map_err(ClientError::Io)?
            == 0
        {
            return Err(ClientError::Protocol(
                "daemon closed the connection without replying".to_string(),
            ));
        }
        serde_json::from_str(response_line.trim_end())
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config_view() -> ConfigView {
        ConfigView {
            vpn_name: "tun0".to_string(),
            vpn_backend: VpnBackend::OpenVpn,
            openvpn_management: "127.0.0.1:7505".to_string(),
            openvpn_management_password_file: Some("/etc/splitway/mgmt.pass".to_string()),
            config_path: "/home/user/.config/splitway/config.json".to_string(),
        }
    }

    #[test]
    fn envelope_round_trip_carries_version() {
        let env = RequestEnvelope::new(Request::AddDomain("example.com".to_string()));
        let json = serde_json::to_string(&env).unwrap();
        let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(
            parsed.request,
            Request::AddDomain("example.com".to_string())
        );
    }

    #[test]
    fn config_verbs_round_trip_in_envelope() {
        // The new GetConfig / SetConfig verbs ride the same versioned envelope.
        for request in [Request::GetConfig, Request::SetConfig(sample_config_view())] {
            let env = RequestEnvelope::new(request.clone());
            let json = serde_json::to_string(&env).unwrap();
            let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.version, PROTOCOL_VERSION);
            assert_eq!(parsed.request, request);
        }
    }

    fn sample_status() -> StatusInfo {
        StatusInfo {
            enabled: true,
            interface: "wg0".to_string(),
            vpn_up: true,
            applied: Some(AppliedInfo {
                interface: "wg0".to_string(),
                domains: vec!["a.com".to_string(), "b.com".to_string()],
                dns_servers: vec!["10.0.0.1".to_string()],
            }),
            routing_state: RoutingState::Applied,
            detected_dns: vec!["10.0.0.1".to_string()],
            detector_health: DetectorHealth::Active,
            domains: vec!["a.com".to_string(), "b.com".to_string()],
        }
    }

    #[test]
    fn response_round_trip() {
        let responses = [
            Response::Ok,
            Response::Domains(vec!["a.com".to_string(), "b.com".to_string()]),
            Response::Error("nope".to_string()),
            Response::Status(sample_status()),
            // Not-applied status: `applied` is None, with a non-Applied state and
            // a failed detector (exercises every new field's other shape).
            Response::Status(StatusInfo {
                enabled: true,
                interface: "tun0".to_string(),
                vpn_up: false,
                applied: None,
                routing_state: RoutingState::VpnDown,
                detected_dns: vec![],
                detector_health: DetectorHealth::Error("nm absent".to_string()),
                domains: vec![],
            }),
            Response::Config(sample_config_view()),
            Response::Interfaces(vec![
                InterfaceInfo {
                    name: "tun0".to_string(),
                    up: true,
                    vpn_like: true,
                },
                InterfaceInfo {
                    name: "lo".to_string(),
                    up: true,
                    vpn_like: false,
                },
            ]),
            // Covered + resolved (Linux-strong: a link is attributed).
            Response::DomainCheck(DomainCheckInfo {
                host: "vault.sub.example.com".to_string(),
                covered: true,
                matched_domain: Some("sub.example.com".to_string()),
                vpn_interface: "tun0".to_string(),
                resolution: Some(ResolutionInfo {
                    addresses: vec!["10.0.0.1".to_string(), "2001:db8::1".to_string()],
                    via_interface: Some("tun0".to_string()),
                    via_dns: None,
                }),
                enabled: true,
                vpn_up: true,
                routing_state: RoutingState::Applied,
            }),
            // Not covered + resolution unavailable (the other shape of each field).
            Response::DomainCheck(DomainCheckInfo {
                host: "example.org".to_string(),
                covered: false,
                matched_domain: None,
                vpn_interface: String::new(),
                resolution: None,
                enabled: false,
                vpn_up: false,
                routing_state: RoutingState::NoDomains,
            }),
            // Verify, in-sync: a populated live state with a matching belief
            // (default-route disabled — the correct post-apply state).
            Response::Verify(VerifyInfo {
                live: LinkDnsState {
                    servers: vec!["10.0.0.1".to_string()],
                    routing_domains: vec!["example.com".to_string()],
                    default_route: Some(false),
                },
                drift: DriftVerdict::InSync,
            }),
            // Verify, drifted: the other shape, naming what diverged — here a
            // catch-all leak (the link is the default route) on top of a missing
            // server and an unrouted domain.
            Response::Verify(VerifyInfo {
                live: LinkDnsState {
                    servers: vec!["198.51.100.1".to_string()],
                    routing_domains: vec![],
                    default_route: Some(true),
                },
                drift: DriftVerdict::Drifted {
                    missing_servers: vec!["10.0.0.1".to_string()],
                    unrouted_domains: vec!["corp.example.com".to_string()],
                    default_route_leak: true,
                },
            }),
            // Verify, not-applicable: an empty live state and nothing believed.
            Response::Verify(VerifyInfo {
                live: LinkDnsState::default(),
                drift: DriftVerdict::NotApplicable,
            }),
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            let parsed: Response = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, response);
        }
    }

    #[test]
    fn list_interfaces_verb_round_trips_in_envelope() {
        let env = RequestEnvelope::new(Request::ListInterfaces);
        let json = serde_json::to_string(&env).unwrap();
        let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(parsed.request, Request::ListInterfaces);
    }

    #[test]
    fn check_domain_verb_round_trips_in_envelope() {
        let env = RequestEnvelope::new(Request::CheckDomain(
            "https://vault.sub.example.com/x".to_string(),
        ));
        let json = serde_json::to_string(&env).unwrap();
        let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(
            parsed.request,
            Request::CheckDomain("https://vault.sub.example.com/x".to_string())
        );
    }

    #[test]
    fn verify_verb_round_trips_in_envelope() {
        let env = RequestEnvelope::new(Request::Verify);
        let json = serde_json::to_string(&env).unwrap();
        let parsed: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, PROTOCOL_VERSION);
        assert_eq!(parsed.request, Request::Verify);
    }

    fn believed(servers: &[&str], domains: &[&str]) -> AppliedInfo {
        AppliedInfo {
            interface: "tun0".to_string(),
            domains: domains.iter().map(|s| s.to_string()).collect(),
            dns_servers: servers.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn live(servers: &[&str], domains: &[&str]) -> LinkDnsState {
        LinkDnsState {
            servers: servers.iter().map(|s| s.to_string()).collect(),
            routing_domains: domains.iter().map(|s| s.to_string()).collect(),
            // The default-route flag is orthogonal to the server/domain dimensions
            // these helpers exercise; `None` ("read-back did not learn it") is the
            // neutral value that never trips the catch-all-leak check. Leak tests
            // set it explicitly via `LinkDnsState { default_route: .., ..live(..) }`.
            default_route: None,
        }
    }

    #[test]
    fn drift_not_applicable_when_nothing_applied() {
        // No belief → nothing to verify, regardless of what is live.
        assert_eq!(
            compare_drift(&live(&["10.0.0.1"], &["example.com"]), None),
            DriftVerdict::NotApplicable
        );
    }

    #[test]
    fn drift_in_sync_when_live_matches_belief() {
        let applied = believed(&["10.0.0.1", "10.0.0.2"], &["example.com"]);
        assert_eq!(
            compare_drift(
                &live(&["10.0.0.1", "10.0.0.2"], &["example.com"]),
                Some(&applied)
            ),
            DriftVerdict::InSync
        );
    }

    #[test]
    fn drift_bare_vs_tilde_and_case_are_not_false_drift() {
        // The parser strips a leading `~`, so the live domain arrives bare; case
        // and a trailing dot also must not count as drift (normalized compare).
        let applied = believed(&["2001:DB8::1"], &["Example.COM"]);
        let live = live(&["2001:db8::1"], &["example.com."]);
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn server_matches_ignores_scope_port_and_sni_decorations() {
        // The live parser strips `%ifname` / `:port` / `#SNI`; the believed side
        // (from the detector) may still carry them. Both reduce to the same bare
        // IP, so they are the same resolver — not drift.
        assert!(server_matches("fe80::1", "fe80::1%wg0"));
        assert!(server_matches("10.0.0.1", "10.0.0.1:53"));
        assert!(server_matches(
            "2001:db8::1",
            "[2001:db8::1]:853#dns.example.com"
        ));
        // Genuinely different resolvers still differ.
        assert!(!server_matches("10.0.0.1", "10.0.0.2"));
        // A non-address token falls back to a folded string compare.
        assert!(!server_matches("10.0.0.1", "not-an-ip"));
        assert!(server_matches("not-an-ip", "not-an-ip"));
    }

    #[test]
    fn drift_believed_route_only_marker_matches_stripped_live_domain() {
        // A believed domain configured with systemd's route-only `~` marker (a
        // hand-edited config, or `add_domain` accepting it) matches the live
        // `corp.example.com` the parser already stripped — not false drift.
        let applied = believed(&["10.0.0.1"], &["~corp.example.com"]);
        let live = live(&["10.0.0.1"], &["corp.example.com"]);
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_scoped_believed_server_matches_bare_live_server() {
        // A believed scoped IPv6 resolver (the detector kept the `%ifname`) vs the
        // live token the parser stripped to bare — the same resolver, so in sync,
        // not a false `Drifted` with the server listed as missing.
        let applied = believed(&["fe80::1%wg0"], &["example.com"]);
        let live = live(&["fe80::1"], &["example.com"]);
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_server_zero_compression_is_not_false_drift() {
        // The same IPv6 address in expanded vs zero-compressed form is one server.
        let applied = believed(&["2001:db8:0:0:0:0:0:1"], &["example.com"]);
        let live = live(&["2001:db8::1"], &["example.com"]);
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_live_parent_domain_covers_believed_subdomain() {
        // The live link routes a broader parent; a believed subdomain is still
        // covered (suffix-aware), so this is in sync, not drift.
        let applied = believed(&["10.0.0.1"], &["sub.example.com"]);
        let live = live(&["10.0.0.1"], &["example.com"]);
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_live_route_all_domain_against_narrow_belief_is_a_leak() {
        // A live route-all routing domain (`~.`, parsed to `.`) makes the link a
        // catch-all just as `default_route == Some(true)` does — even when the flag
        // is unknown. Against a narrow belief that is a leak: the believed domains
        // are "covered" (so unrouted is empty), but the link routes everything.
        let applied = believed(&["10.0.0.1"], &["example.com", "corp.example.com"]);
        let live = live(&["10.0.0.1"], &["."]);
        assert_eq!(
            compare_drift(&live, Some(&applied)),
            DriftVerdict::Drifted {
                missing_servers: vec![],
                unrouted_domains: vec![],
                default_route_leak: true,
            }
        );
    }

    #[test]
    fn drift_live_route_all_with_route_all_belief_is_in_sync() {
        // A belief that is itself route-all (a deliberate full-tunnel split) WANTS
        // the live link to be the catch-all, so a live `~.` is in sync, not a leak.
        let applied = believed(&["10.0.0.1"], &["~."]);
        let live = live(&["10.0.0.1"], &["."]);
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_live_child_does_not_cover_believed_parent() {
        // Coverage is suffix-aware and asymmetric: a live child `sub.example.com`
        // does NOT cover a believed parent `example.com`, so the parent is
        // reported unrouted (guards against an over-broad bidirectional match).
        let applied = believed(&["10.0.0.1"], &["example.com"]);
        let live = live(&["10.0.0.1"], &["sub.example.com"]);
        assert_eq!(
            compare_drift(&live, Some(&applied)),
            DriftVerdict::Drifted {
                missing_servers: vec![],
                unrouted_domains: vec!["example.com".to_string()],
                default_route_leak: false,
            }
        );
    }

    #[test]
    fn drift_reports_missing_servers_and_unrouted_domains() {
        let applied = believed(
            &["10.0.0.1", "10.0.0.2"],
            &["a.example.com", "b.example.com"],
        );
        // Live has only one of the two servers and routes only one domain.
        let live = live(&["10.0.0.1"], &["a.example.com"]);
        assert_eq!(
            compare_drift(&live, Some(&applied)),
            DriftVerdict::Drifted {
                missing_servers: vec!["10.0.0.2".to_string()],
                unrouted_domains: vec!["b.example.com".to_string()],
                default_route_leak: false,
            }
        );
    }

    #[test]
    fn drift_empty_live_against_belief_is_full_drift() {
        // An empty live state (e.g. read-back unavailable) against a non-empty
        // belief reports everything as diverged — never a false InSync.
        let applied = believed(&["10.0.0.1"], &["example.com"]);
        assert_eq!(
            compare_drift(&LinkDnsState::default(), Some(&applied)),
            DriftVerdict::Drifted {
                missing_servers: vec!["10.0.0.1".to_string()],
                unrouted_domains: vec!["example.com".to_string()],
                default_route_leak: false,
            }
        );
    }

    #[test]
    fn drift_in_sync_when_belief_is_empty() {
        // A believed-but-empty mapping (nothing to install) is trivially in sync
        // with any live state — including an empty one.
        let applied = believed(&[], &[]);
        assert_eq!(
            compare_drift(&LinkDnsState::default(), Some(&applied)),
            DriftVerdict::InSync
        );
    }

    #[test]
    fn drift_catch_all_default_route_is_a_leak_even_when_domains_present() {
        // The regression this fix targets: the narrow split is technically present
        // (the believed server and domain are both live), but the link is the DNS
        // default route, so it resolves EVERY unmatched name through the VPN — a
        // sibling like `bitbucket.example.com` leaks even though only
        // `jira.example.com` was configured. That is drift, not InSync, with the
        // leak flagged and the per-field lists empty.
        let applied = believed(&["10.0.0.1"], &["jira.example.com"]);
        let live = LinkDnsState {
            default_route: Some(true),
            ..live(&["10.0.0.1"], &["jira.example.com"])
        };
        assert_eq!(
            compare_drift(&live, Some(&applied)),
            DriftVerdict::Drifted {
                missing_servers: vec![],
                unrouted_domains: vec![],
                default_route_leak: true,
            }
        );
    }

    #[test]
    fn drift_default_route_disabled_is_in_sync() {
        // The correct post-apply state: the link resolves only its routing domain,
        // not everything — no leak, so InSync.
        let applied = believed(&["10.0.0.1"], &["jira.example.com"]);
        let live = LinkDnsState {
            default_route: Some(false),
            ..live(&["10.0.0.1"], &["jira.example.com"])
        };
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_unknown_default_route_is_never_a_leak() {
        // `None` means the read-back did not learn the flag (an older peer, a
        // transient failure) or the platform has no link-level catch-all (macOS).
        // Unknown must never fabricate drift, mirroring the read-back-degrades-to-
        // empty ethos — so a matching belief stays InSync.
        let applied = believed(&["10.0.0.1"], &["jira.example.com"]);
        let live = LinkDnsState {
            default_route: None,
            ..live(&["10.0.0.1"], &["jira.example.com"])
        };
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_default_route_with_route_all_belief_is_not_a_leak() {
        // A belief that is itself route-all (a deliberate full-tunnel split, e.g. a
        // hand-edited `~.`) WANTS the link to be the catch-all, so default-route on
        // it is correct, not a leak. Guards against a false positive for that case.
        let applied = believed(&["10.0.0.1"], &["~."]);
        let live = LinkDnsState {
            default_route: Some(true),
            ..live(&["10.0.0.1"], &["."])
        };
        assert_eq!(compare_drift(&live, Some(&applied)), DriftVerdict::InSync);
    }

    #[test]
    fn drift_catch_all_leak_combines_with_missing_servers_and_domains() {
        // All three drift dimensions at once: a missing server, an unrouted domain,
        // AND the link is the catch-all — every field is populated together.
        let applied = believed(&["10.0.0.1", "10.0.0.2"], &["jira.example.com"]);
        let live = LinkDnsState {
            default_route: Some(true),
            ..live(&["10.0.0.1"], &[])
        };
        assert_eq!(
            compare_drift(&live, Some(&applied)),
            DriftVerdict::Drifted {
                missing_servers: vec!["10.0.0.2".to_string()],
                unrouted_domains: vec!["jira.example.com".to_string()],
                default_route_leak: true,
            }
        );
    }

    #[test]
    fn new_status_wire_types_round_trip() {
        // Each new type round-trips on its own, like ConfigView does.
        let applied = AppliedInfo {
            interface: "utun4".to_string(),
            domains: vec!["corp.example".to_string()],
            dns_servers: vec!["10.8.0.1".to_string(), "10.8.0.2".to_string()],
        };
        let json = serde_json::to_string(&applied).unwrap();
        assert_eq!(serde_json::from_str::<AppliedInfo>(&json).unwrap(), applied);

        for state in [
            RoutingState::Disabled,
            RoutingState::NoDomains,
            RoutingState::VpnDown,
            RoutingState::NoDnsFromVpn,
            RoutingState::Applied,
            RoutingState::ApplyFailed,
            RoutingState::ConfigInvalid,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<RoutingState>(&json).unwrap(), state);
        }

        for health in [
            DetectorHealth::Active,
            DetectorHealth::Inactive,
            DetectorHealth::Error("bad management endpoint".to_string()),
        ] {
            let json = serde_json::to_string(&health).unwrap();
            assert_eq!(
                serde_json::from_str::<DetectorHealth>(&json).unwrap(),
                health
            );
        }

        let iface = InterfaceInfo {
            name: "wg0".to_string(),
            up: false,
            vpn_like: true,
        };
        let json = serde_json::to_string(&iface).unwrap();
        assert_eq!(serde_json::from_str::<InterfaceInfo>(&json).unwrap(), iface);
    }

    #[test]
    fn config_view_round_trips() {
        let view = sample_config_view();
        let json = serde_json::to_string(&view).unwrap();
        let parsed: ConfigView = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, view);
    }

    #[test]
    fn config_view_optional_fields_default_when_absent() {
        // Mirror the LocalConfig back-compat discipline: a peer that omits the
        // optional fields still parses, with the defaults applied.
        let json = r#"{"vpn_name":"wg0"}"#;
        let parsed: ConfigView = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.vpn_name, "wg0");
        assert_eq!(parsed.vpn_backend, VpnBackend::NetworkManager);
        assert!(parsed.openvpn_management.is_empty());
        assert!(parsed.openvpn_management_password_file.is_none());
        assert!(parsed.config_path.is_empty());
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        // A non-empty XDG_RUNTIME_DIR places the socket directly under it.
        assert_eq!(
            socket_path_from(Some(OsString::from("/run/user/1000"))),
            PathBuf::from("/run/user/1000/splitway.sock")
        );
    }

    #[test]
    fn socket_path_falls_back_to_system_dir_without_xdg() {
        // An unset or empty XDG_RUNTIME_DIR falls back to the system socket dir.
        let expected = PathBuf::from(SYSTEM_SOCKET_DIR).join("splitway.sock");
        assert_eq!(socket_path_from(None), expected);
        assert_eq!(socket_path_from(Some(OsString::new())), expected);
    }
}
