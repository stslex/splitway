// Splitway's daemon and IPC are Unix-only (Unix domain sockets, POSIX
// signals, resolvectl). Windows support is deferred (see ROADMAP.md); the
// crate still compiles there — via the stub `main` below — so the
// cross-platform release matrix stays green.
#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::exit;

#[cfg(unix)]
use splitway_shared::config;
#[cfg(unix)]
use splitway_shared::ipc::{self, Request, Response};

#[cfg(unix)]
use crate::command::{Command, CommandParser};

#[cfg(unix)]
mod backend;
#[cfg(unix)]
mod command;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod detector;
// Tool resolution + loud-on-missing spawn wrapper for the external commands the
// daemon shells out to (`nmcli`, `resolvectl`). Linux-only today — the macOS
// backend can adopt it later (see `exec` docs / ROADMAP.md).
#[cfg(target_os = "linux")]
mod exec;
#[cfg(unix)]
mod interfaces;

#[cfg(unix)]
fn main() {
    // Default to `info` when RUST_LOG is unset. Plain `env_logger::init()` keeps
    // env_logger's built-in default of `error`, which suppresses the very `warn`
    // and `info` lines that diagnose a broken deployment — e.g. a detect failure
    // or "VPN up" transition. A core-function failure must be visible without the
    // operator first knowing to set RUST_LOG. RUST_LOG still overrides this.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match env::args().parse_command() {
        Ok(Command::Run {
            config,
            socket_group,
        }) => daemon::run(config, socket_group),
        Ok(Command::Status) => status(),
        Ok(Command::Revert { config }) => revert(config),
        Err(message) => {
            eprintln!("{message}");
            exit(2);
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("splitway-daemon is only supported on Unix platforms (Linux/macOS)");
    std::process::exit(1);
}

/// Quick one-shot status: query the running daemon over IPC. If no daemon is
/// running, say so (it does not fall back to a direct backend read).
#[cfg(unix)]
fn status() {
    match ipc::client::send_request(Request::Status) {
        Ok(Response::Status(info)) => {
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
        Ok(other) => {
            eprintln!("unexpected response from daemon: {other:?}");
            exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    }
}

/// Emergency one-shot revert straight to the backend. Works even with no
/// daemon running — an escape hatch. Distinct from `splitway-cli disable`,
/// which tells a *running* daemon to stop applying and persists that choice.
#[cfg(unix)]
fn revert(config_path: Option<PathBuf>) {
    let config_path = config_path.unwrap_or_else(config::config_file_path);
    let interface = match config::load_config_from(&config_path) {
        Ok(config) => config.vpn_name,
        Err(e) => {
            eprintln!("failed to read config: {e}");
            exit(1);
        }
    };
    if interface.is_empty() {
        eprintln!("no vpn_name configured to revert");
        exit(1);
    }

    let backend = backend::create_dns_backend();
    match backend.revert_rules(&interface) {
        Ok(()) => println!("reverted DNS rules on {interface}"),
        Err(e) => {
            eprintln!("revert failed on {interface}: {e}");
            exit(1);
        }
    }
}
