use std::process::exit;

use splitway_shared::config::{get_config, ConfigParseError};

mod backend;

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str());

    match command {
        Some("run") => launch_daemon(),
        Some("revert") => revert_dns_domain(),
        Some("status") => show_status(),
        _ => println!("usage: splitway-daemon <apply|revert|status>"),
    }
}

fn show_status() {
    let vpn_name = get_config().map_or("default".to_string(), |config| config.vpn_name.clone());
    let backend = backend::create_backend();

    if let Err(e) = backend.status(&vpn_name) {
        panic!("error show_status: {e}");
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

    let backend = backend::create_backend();

    let vpn_info = match backend.detect_vpn(&local_config.vpn_name) {
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

    let backend = backend::create_backend();

    if let Err(e) = backend.revert_rules(&name) {
        println!("error revert_dns_domain: {e}");
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
