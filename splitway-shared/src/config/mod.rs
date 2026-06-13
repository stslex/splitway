use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub fn get_config() -> Result<LocalConfig, ConfigParseError> {
    load_config_from(&config_file_path())
}

/// Read and parse the config at `path`. Used by both the daemon's startup
/// and its `ReloadConfig` handler; takes an explicit path so it is unit
/// testable without touching the real config location.
pub fn load_config_from(path: &Path) -> Result<LocalConfig, ConfigParseError> {
    log::info!("Config path: {}", path.display());
    let config_str = fs::read_to_string(path).map_err(|e| {
        log::error!("Error read config file: {e}");
        ConfigParseError::ConfigNotFound
    })?;
    serde_json::from_str::<LocalConfig>(&config_str).map_err(|e| {
        log::error!("Error deserialize: {e}");
        ConfigParseError::SerializeError
    })
}

pub fn create_empty_config() -> Result<(), ConfigParseError> {
    let empty_config = LocalConfig {
        vpn_name: String::new(),
        vpn_hosts: Vec::new(),
        enabled: default_enabled(),
    };
    save_config(&empty_config)
}

/// Persist `config` to the real config location atomically.
pub fn save_config(config: &LocalConfig) -> Result<(), ConfigParseError> {
    save_config_to(&config_file_path(), config)
}

/// Persist `config` to `path` atomically. Separate from [`save_config`] so
/// it can be unit tested against a temp path.
pub fn save_config_to(path: &Path, config: &LocalConfig) -> Result<(), ConfigParseError> {
    let json = serde_json::to_vec_pretty(config).map_err(|e| {
        log::error!("Error serialize config: {e}");
        ConfigParseError::SerializeError
    })?;
    atomic_write(path, &json).map_err(|e| {
        log::error!("Error write config {}: {e}", path.display());
        ConfigParseError::WriteError(e.to_string())
    })
}

/// Write `contents` to `path` atomically: write a sibling temp file, fsync
/// it, then rename it over `path`. A crash mid-write leaves either the old
/// file or the complete new file behind — never a truncated config.
///
/// Callers serialize their writes (the daemon's single state-owner task),
/// so the fixed `.tmp` sibling never races another writer.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "config path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;

    // Best-effort: fsync the directory so the rename itself is durable.
    if let Ok(dir_file) = File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

pub fn config_file_path() -> PathBuf {
    config_folder_path().join("config.json")
}

fn config_folder_path() -> PathBuf {
    PathBuf::from(format!(
        "{}/.config/splitway",
        std::env::var("HOME").unwrap()
    ))
}

fn default_enabled() -> bool {
    true
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    pub vpn_name: String,
    pub vpn_hosts: Vec<String>,
    /// Whether the daemon applies rules. Defaults to `true` so configs
    /// written before this field existed keep applying as before.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug)]
pub enum ConfigParseError {
    ConfigNotFound,
    SerializeError,
    WriteError(String),
}

impl Display for ConfigParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigParseError::ConfigNotFound => write!(f, "Config file not found"),
            ConfigParseError::SerializeError => write!(f, "Error serialize/deserialize config"),
            ConfigParseError::WriteError(e) => write!(f, "Error writing config: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp file path for a test, created under a fresh directory.
    fn temp_config(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("splitway-config-test-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[test]
    fn local_config_serde_round_trip() {
        let config = LocalConfig {
            vpn_name: "wg0".to_string(),
            vpn_hosts: vec!["example.com".to_string(), "internal.corp".to_string()],
            enabled: false,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: LocalConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, config);
    }

    #[test]
    fn enabled_defaults_to_true_when_absent() {
        // Configs predating the `enabled` field must still parse.
        let json = r#"{"vpn_name":"wg0","vpn_hosts":["a.com"]}"#;
        let parsed: LocalConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.vpn_name, "wg0");
    }

    #[test]
    fn atomic_write_then_read_back() {
        let path = temp_config("atomic-write");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");

        // Overwriting replaces the contents atomically.
        atomic_write(&path, b"world!").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "world!");

        // No temp file is left behind.
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn save_config_to_round_trips() {
        let path = temp_config("save-round-trip");
        let config = LocalConfig {
            vpn_name: "tun0".to_string(),
            vpn_hosts: vec!["corp.example".to_string()],
            enabled: true,
        };
        save_config_to(&path, &config).unwrap();
        let loaded = load_config_from(&path).unwrap();
        assert_eq!(loaded, config);
    }
}
