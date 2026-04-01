use crate::config::{ConfigController, ResolvedConfig};
use splitway_shared::config::LocalConfig;
use std::process::Command;

impl ConfigController for LocalConfig {
    fn resolve(&self) -> Option<ResolvedConfig> {
        resolve_dns(&self.vpn_name).map(|dns| ResolvedConfig {
            vpn_name: self.vpn_name.clone(),
            vpn_ip: dns,
            vpn_hosts: self.vpn_hosts.clone(),
        })
    }
}

fn resolve_dns<'a>(vpn_name: &'a str) -> Option<String> {
    let output = Command::new("nmcli")
        .args(["device", "show", vpn_name])
        .output()
        .ok()?;

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.contains("DNS"))
        .and_then(|line| line.split_whitespace().last())
        .map(|ip| ip.to_string())
}
