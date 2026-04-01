mod controller;

pub trait ConfigController {
    fn resolve(&self) -> Option<ResolvedConfig>;
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub vpn_name: String,
    pub vpn_ip: String,
    pub vpn_hosts: Vec<String>,
}
