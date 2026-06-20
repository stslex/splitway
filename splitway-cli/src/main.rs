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
        Response::Error(message) => eprintln!("error: {message}"),
    }
}
