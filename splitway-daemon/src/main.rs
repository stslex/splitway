// Splitway's daemon and IPC are Unix-only (Unix domain sockets, POSIX
// signals, resolvectl). Windows support is deferred (see ROADMAP.md); the
// crate still compiles there — via the stub `main` below — so the
// cross-platform release matrix stays green.
#[cfg(unix)]
use std::env;
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

#[cfg(unix)]
fn main() {
    env_logger::init();
    match env::args().parse_command() {
        Ok(Command::Run) => daemon::run(),
        Ok(Command::Status) => status(),
        Ok(Command::Revert) => revert(),
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
            println!("applied:   {}", info.applied);
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
fn revert() {
    let interface = match config::get_config() {
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
