use std::env;
use std::process::exit;

use crate::command::{Command, CommandParser};
use splitway_shared::config::{get_config, ConfigParseError};

mod backend;
mod command;
mod detector;

fn main() {
    env_logger::init();
    match env::args().parse_command() {
        Command::Run => launch_daemon(),
        Command::Revert => revert_dns_domain(),
        Command::Status => show_status(),
        Command::Watch => watch_vpn(),
    }
}

/// Debug subcommand: log VPN up/down events until Ctrl-C.
/// Owns the tokio runtime; the daemon itself stays sync until Phase 2.
fn watch_vpn() {
    let vpn_name = match get_config() {
        Ok(config) => config.vpn_name,
        Err(e) => {
            handle_config_error(&e);
            exit(1);
        }
    };

    let detector = detector::create_vpn_detector();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("failed to build tokio runtime: {e}");
            exit(1);
        }
    };

    runtime.block_on(async move {
        let mut events = match detector.watch(&vpn_name) {
            Ok(events) => events,
            Err(e) => {
                log::error!("failed to start VPN watch for {vpn_name}: {e}");
                exit(1);
            }
        };

        log::info!("watching VPN events for {vpn_name}; press Ctrl-C to stop");
        while let Some(event) = events.recv().await {
            log::info!("VPN event: {event:?}");
        }
        log::warn!("VPN event stream for {vpn_name} ended");
    });
}

fn show_status() {
    let vpn_name = get_config().map_or("default".to_string(), |config| config.vpn_name.clone());
    let backend = backend::create_dns_backend();

    if let Err(e) = backend.status(&vpn_name) {
        log::error!("error show_status: {e}");
        exit(1);
    }
}

fn launch_daemon() {
    let local_config = match get_config() {
        Ok(config) => config,
        Err(e) => {
            handle_config_error(&e);
            exit(1);
        }
    };

    let backend = backend::create_dns_backend();
    let detector = detector::create_vpn_detector();

    let vpn_info = match detector.detect(&local_config.vpn_name) {
        Ok(info) => info,
        Err(e) => {
            log::error!("Failed to detect VPN: {e}");
            exit(1);
        }
    };

    log::info!("Detected VPN: {:?}", vpn_info);

    if let Err(e) = backend.apply_rules(&vpn_info, &local_config.vpn_hosts) {
        log::error!("Failed to apply rules: {e}");
        exit(1);
    }
}

fn revert_dns_domain() {
    let name = match get_config() {
        Ok(config) => config.vpn_name,
        Err(e) => {
            handle_config_error(&e);
            exit(1);
        }
    };

    let backend = backend::create_dns_backend();

    if let Err(e) = backend.revert_rules(&name) {
        log::error!("error revert_dns_domain: {e}");
    }
}

fn handle_config_error(e: &ConfigParseError) {
    match e {
        ConfigParseError::ConfigNotFound => {
            log::error!("Config file not found, creating empty config");
            if let Err(e) = splitway_shared::config::create_empty_config() {
                log::error!("Error create empty config: {e}");
            }
        }
        _ => log::error!("Error get config: {e}"),
    }
}
