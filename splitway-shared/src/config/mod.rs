use std::{fmt::Display, fs::read_to_string};

use serde::{Deserialize, Serialize};

pub fn get_config() -> Result<LocalConfig, ConfigParseError> {
    let path = config_path();
    log::info!("Config path: {path}");
    let confi_str = read_to_string(path).map_err(|e| {
        log::error!("Error read config file: {e}");
        ConfigParseError::ConfigNotFound
    })?;
    serde_json::from_str::<LocalConfig>(confi_str.as_str()).map_err(|e| {
        log::error!("Error deserialize: {e}");
        ConfigParseError::SerializeError
    })
}

pub fn create_empty_config<'a>() -> Result<(), ConfigParseError> {
    std::fs::create_dir_all(config_folder_path()).map_err(|e| {
        log::error!("Error create config dir: {e}");
        ConfigParseError::ConfigNotFound
    })?;
    let empty_config = LocalConfig {
        vpn_name: String::new(),
        vpn_hosts: Vec::new(),
    };
    let empty_config_str = serde_json::to_string(&empty_config).map_err(|e| {
        log::error!("Error serialize: {e}");
        ConfigParseError::SerializeError
    })?;
    std::fs::write(config_path(), empty_config_str).map_err(|e| {
        log::error!("Error write config file: {e}");
        ConfigParseError::ConfigNotFound
    })?;
    Ok(())
}

fn config_path() -> String {
    config_folder_path() + "/config.json"
}

fn config_folder_path() -> String {
    format!("{}/.config/splitway", std::env::var("HOME").unwrap())
}

#[derive(Deserialize, Serialize, Debug)]
pub struct LocalConfig {
    pub vpn_name: String,
    pub vpn_hosts: Vec<String>,
}

#[derive(Debug)]
pub enum ConfigParseError {
    ConfigNotFound,
    SerializeError,
    Unresolve,
}

impl Display for ConfigParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigParseError::ConfigNotFound => write!(f, "Config file not found"),
            ConfigParseError::SerializeError => write!(f, "Error serialize/deserialize config"),
            ConfigParseError::Unresolve => write!(f, "Error unresolve config"),
        }
    }
}
