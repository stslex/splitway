use std::process::{exit, Command};

use splitway_shared::config::{get_config, ConfigParseError};

use crate::config::{ConfigController, ResolvedConfig};

mod config;

fn main() {
    env_logger::init();
    let config = get_resolved_config();

    log::info!("Resolved config: {:?}", config);

    call_resolvectl(&config);
    add_dns_domain(&config);
}

fn get_resolved_config() -> ResolvedConfig {
    match get_config() {
        Ok(config) => config,
        Err(e) => {
            match e {
                ConfigParseError::ConfigNotFound => {
                    log::error!("Config file not found, creating empty config");
                    if let Err(e) = splitway_shared::config::create_empty_config() {
                        log::error!("Error create empty config: {e}");
                    }
                }
                _ => log::error!("Error get config: {e}"),
            }
            exit(1);
        }
    }
    .resolve()
    .unwrap()
}

fn add_dns_domain(config: &ResolvedConfig) {
    let result = Command::new("/usr/bin/resolvectl")
        .arg("domain")
        .arg(config.vpn_name.clone())
        .args(&config.vpn_hosts)
        .output();
    match result {
        Ok(output) => {
            println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
            println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => println!("error add_dns_domain:: {e}"),
    }
}

fn call_resolvectl(config: &ResolvedConfig) {
    let result = Command::new("/usr/bin/resolvectl")
        .arg("dns")
        .arg(config.vpn_name.clone())
        .arg(config.vpn_ip.clone())
        .output();
    match result {
        Ok(output) => {
            println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
            println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => println!("error call_resolvectl: {e}"),
    }
}

fn revert_dns_domain(name: String) {
    let result = Command::new("/usr/bin/resolvectl")
        .arg("revert")
        .arg(name)
        .output();
    match result {
        Ok(output) => {
            println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
            println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => println!("error revert_dns_domain: {e}"),
    }
}
