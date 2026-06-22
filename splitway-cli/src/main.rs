//! `splitway` CLI: a thin, single-shot client for the daemon's IPC socket.
//! It holds no daemon logic — it parses a subcommand, sends one request,
//! prints the reply, and exits.
//!
//! The IPC client is Unix-only (Unix domain socket). On non-Unix the binary
//! still builds — via the stub path in `main` — so the cross-platform release
//! matrix stays green; see ROADMAP.md.

#[cfg(unix)]
use clap::{Parser, Subcommand};

#[cfg(unix)]
#[derive(Parser)]
#[command(
    name = "splitway",
    about = "Control the splitway daemon over its IPC socket"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[cfg(unix)]
#[derive(Subcommand)]
enum Commands {
    /// Show daemon and DNS routing status.
    Status,
    /// Enable rule application (persisted).
    Enable,
    /// Disable rule application and revert (persisted).
    Disable,
    /// Add a domain to route through the VPN.
    Add { domain: String },
    /// Remove a domain.
    Remove { domain: String },
    /// List the configured domains.
    List,
    /// Reload the daemon's config from disk.
    Reload,
    /// Check whether a domain (a URL or bare host) routes through the VPN.
    Check { url_or_host: String },
    /// Read the live DNS state back from the system and report any drift from
    /// what the daemon believes it installed.
    Verify,
}

#[cfg(unix)]
fn main() {
    // Parse only on Unix: the unsupported-platform path below must print its own
    // message deterministically rather than letting clap exit first on a
    // missing/invalid argument.
    let cli = Cli::parse();
    run(cli);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("splitway is only supported on Unix platforms (Linux/macOS)");
    std::process::exit(1);
}

#[cfg(unix)]
fn run(cli: Cli) {
    use splitway_shared::ipc::{self, Request};

    let request = match cli.command {
        Commands::Status => Request::Status,
        Commands::Enable => Request::Enable,
        Commands::Disable => Request::Disable,
        Commands::Add { domain } => Request::AddDomain(domain),
        Commands::Remove { domain } => Request::RemoveDomain(domain),
        Commands::List => Request::ListDomains,
        Commands::Reload => Request::ReloadConfig,
        Commands::Check { url_or_host } => Request::CheckDomain(url_or_host),
        Commands::Verify => Request::Verify,
    };

    match ipc::client::send_request(request) {
        Ok(response) => {
            print_response(&response);
            if is_error(&response) {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn is_error(response: &splitway_shared::ipc::Response) -> bool {
    matches!(response, splitway_shared::ipc::Response::Error(_))
}

#[cfg(unix)]
fn print_response(response: &splitway_shared::ipc::Response) {
    use splitway_shared::ipc::Response;

    match response {
        Response::Ok => println!("ok"),
        Response::Status(info) => {
            println!("enabled:   {}", info.enabled);
            println!("interface: {}", info.interface);
            println!("vpn_up:    {}", info.vpn_up);
            println!("routing:   {}", info.routing_state);
            println!(
                "applied:   {}",
                match &info.applied {
                    Some(applied) => applied.to_string(),
                    None => "(none)".to_string(),
                }
            );
            println!(
                "detected:  {}",
                if info.detected_dns.is_empty() {
                    "(none)".to_string()
                } else {
                    info.detected_dns.join(", ")
                }
            );
            println!("detector:  {}", info.detector_health);
            println!(
                "domains:   {}",
                if info.domains.is_empty() {
                    "(none)".to_string()
                } else {
                    info.domains.join(", ")
                }
            );
        }
        Response::Domains(domains) => {
            if domains.is_empty() {
                println!("(no domains configured)");
            } else {
                for domain in domains {
                    println!("{domain}");
                }
            }
        }
        // The CLI has no get/set-config subcommand, so it never sends
        // `GetConfig` and should not receive this. Render it defensively rather
        // than panic, so a future peer that does reply with it stays readable.
        Response::Config(view) => {
            println!("vpn_name:    {}", view.vpn_name);
            // Canonical kebab-case token (matches config / IPC), not Debug.
            println!("vpn_backend: {}", view.vpn_backend.as_str());
            println!("openvpn.management: {}", view.openvpn_management);
            println!(
                "openvpn.management_password_file: {}",
                view.openvpn_management_password_file
                    .as_deref()
                    .unwrap_or("(none)")
            );
            println!("config_path: {}", view.config_path);
        }
        // The CLI has no interface-listing subcommand, so it never sends
        // `ListInterfaces`. Render defensively (like `Config`) rather than panic,
        // so a future peer that replies with it stays readable.
        Response::Interfaces(interfaces) => {
            if interfaces.is_empty() {
                println!("(no interfaces found)");
            } else {
                for iface in interfaces {
                    let up = if iface.up { "up" } else { "down" };
                    let vpn = if iface.vpn_like { ", vpn-like" } else { "" };
                    println!("{} ({up}{vpn})", iface.name);
                }
            }
        }
        Response::DomainCheck(info) => print_domain_check(info),
        Response::Verify(info) => print_verify(info),
        Response::Error(message) => eprintln!("error: {message}"),
    }
}

/// Render a [`Response::Verify`] plainly: the live per-link DNS state read back
/// from the system, then the drift verdict comparing it to what the daemon
/// believes it installed. Careful to read as *reality vs belief* — and, like the
/// route-check, never to imply reachability (Splitway governs DNS, not IP
/// routing). Status stays the daemon's belief; `verify` is the explicit reality
/// check.
#[cfg(unix)]
fn print_verify(info: &splitway_shared::ipc::VerifyInfo) {
    use splitway_shared::ipc::DriftVerdict;

    let live = &info.live;
    let live_empty = live.servers.is_empty() && live.routing_domains.is_empty();

    println!(
        "live DNS servers: {}",
        if live.servers.is_empty() {
            "(none)".to_string()
        } else {
            live.servers.join(", ")
        }
    );
    println!(
        "live DNS domains: {}",
        if live.routing_domains.is_empty() {
            "(none)".to_string()
        } else {
            live.routing_domains.join(", ")
        }
    );

    match &info.drift {
        DriftVerdict::NotApplicable => {
            println!("verdict:          nothing is applied right now — the daemon believes no DNS");
            println!("                  rules are installed, so there is nothing to verify");
            println!(
                "                  (the lines above are the link's current DNS state, if any)"
            );
        }
        DriftVerdict::InSync => {
            println!(
                "verdict:          in sync — the live DNS state matches what the daemon installed"
            );
        }
        DriftVerdict::Drifted {
            missing_servers,
            unrouted_domains,
        } => {
            println!(
                "verdict:          DRIFT — the live DNS state diverges from what the daemon believes"
            );
            println!("                  it installed:");
            if !missing_servers.is_empty() {
                println!(
                    "                  missing servers (believed, absent live): {}",
                    missing_servers.join(", ")
                );
            }
            if !unrouted_domains.is_empty() {
                println!(
                    "                  unrouted domains (believed, not routed live): {}",
                    unrouted_domains.join(", ")
                );
            }
            if live_empty {
                println!("                  (the live read came back empty — the read-back may be");
                println!(
                    "                   unavailable on this system, or the link no longer carries"
                );
                println!("                   Splitway's DNS)");
            }
        }
    }

    println!();
    println!("note: this checks DNS only — what the link's resolver state IS versus what the");
    println!("      daemon installed, not whether the resolved addresses are reachable through");
    println!("      the tunnel.");
}

/// Render a [`Response::DomainCheck`] plainly. Distinguishes coverage (is the
/// host matched by a configured routing domain) from live resolution (which link
/// actually answered), and is careful never to read "resolved" as "reachable" —
/// Splitway governs DNS, not IP routing.
#[cfg(unix)]
fn print_domain_check(info: &splitway_shared::ipc::DomainCheckInfo) {
    println!("host:      {}", info.host);

    if info.covered {
        match &info.matched_domain {
            Some(domain) => println!("coverage:  covered by the configured domain `{domain}`"),
            None => println!("coverage:  covered"),
        }
        // Coverage means "configured to route"; whether it is routed *right now*
        // is the daemon's routing_state (its belief about whether usable
        // split-DNS rules are installed) — not just enabled && vpn_up, which the
        // VPN-up-but-no-DNS and apply-failed states would falsely read as routed.
        use splitway_shared::ipc::RoutingState;
        let iface = if info.vpn_interface.is_empty() {
            "the VPN".to_string()
        } else {
            format!("the VPN ({})", info.vpn_interface)
        };
        let routing = match info.routing_state {
            RoutingState::Applied => "routed through the VPN's DNS".to_string(),
            RoutingState::Disabled => {
                "configured to route, but rule application is disabled — not routed right now"
                    .to_string()
            }
            RoutingState::VpnDown => {
                format!("configured to route, but {iface} is down — not routed right now")
            }
            RoutingState::NoDnsFromVpn => {
                format!("configured to route, but {iface} is up and pushes no DNS — not routed right now")
            }
            RoutingState::ApplyFailed => {
                "configured to route, but applying the rules failed (out of sync) — not routed right now"
                    .to_string()
            }
            RoutingState::ConfigInvalid => {
                "the config file on disk is invalid; routing reflects the last-good config".to_string()
            }
            // Coverage implies a configured domain exists, so NoDomains is not
            // expected here; render defensively rather than claim it is routed.
            RoutingState::NoDomains => {
                "configured to route, but no domains are configured — not routed right now".to_string()
            }
        };
        println!("routing:   {routing}");
    } else {
        println!("coverage:  NOT covered by any configured domain");
        println!("           add it with:  splitway add {}", info.host);
    }

    match &info.resolution {
        Some(resolution) => {
            let addrs = if resolution.addresses.is_empty() {
                "(none)".to_string()
            } else {
                resolution.addresses.join(", ")
            };
            println!("resolved:  {addrs}");
            match &resolution.via_interface {
                // Resolved via the configured VPN link → routed through the VPN's DNS.
                Some(link) if !info.vpn_interface.is_empty() && link == &info.vpn_interface => {
                    println!("via link:  {link} — resolved through the VPN's DNS");
                }
                // Resolved via some other link → a drift hint, surfaced factually.
                Some(link) => {
                    let configured = if info.vpn_interface.is_empty() {
                        "no VPN interface is configured".to_string()
                    } else {
                        format!("not the configured VPN interface `{}`", info.vpn_interface)
                    };
                    println!(
                        "via link:  {link} — {configured}; this name is not resolving through the VPN right now"
                    );
                }
                // No attribution (best-effort platforms, e.g. macOS).
                None => println!("via link:  (the platform does not report which link answered)"),
            }
            if let Some(dns) = &resolution.via_dns {
                println!("via dns:   {dns}");
            }
        }
        None => println!("resolved:  (live resolution unavailable)"),
    }

    // The `routing:` line is the daemon's *belief* (it thinks rules are applied);
    // the `via link:` line is *reality* (where the live query was answered). When
    // those genuinely disagree — believed-applied but answered by a non-VPN link —
    // frame it as drift so the two lines don't read as a bare contradiction. The
    // live answer is authoritative; `splitway verify` surfaces read-back/drift
    // for the whole link explicitly (reconcile-on-drift remains future).
    let drifting = matches!(
        info.routing_state,
        splitway_shared::ipc::RoutingState::Applied
    ) && info
        .resolution
        .as_ref()
        .and_then(|r| r.via_interface.as_deref())
        .is_some_and(|link| !info.vpn_interface.is_empty() && link != info.vpn_interface);
    if drifting {
        println!(
            "drift:     'routing' (the daemon's belief) and 'via link' (the live result) disagree — trust the live result"
        );
    }

    println!();
    println!("note: this checks DNS only — whether the name resolves through the VPN's");
    println!("      resolver, not whether the resolved address is reachable through the tunnel.");
}
