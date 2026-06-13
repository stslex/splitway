use std::env;
use std::process::exit;

use splitway_shared::config;
use splitway_shared::ipc::{self, Request, Response};

use crate::command::{Command, CommandParser};

mod backend;
mod command;
mod daemon;
mod detector;

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

/// Quick one-shot status: query the running daemon over IPC. If no daemon is
/// running, say so (it does not fall back to a direct backend read).
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
