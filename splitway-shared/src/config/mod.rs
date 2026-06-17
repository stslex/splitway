use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub fn get_config() -> Result<LocalConfig, ConfigParseError> {
    load_config_from(&config_file_path())
}

/// Read and parse the config at `path`. Used by both the daemon's startup
/// and its `ReloadConfig` handler; takes an explicit path so it is unit
/// testable without touching the real config location.
pub fn load_config_from(path: &Path) -> Result<LocalConfig, ConfigParseError> {
    log::info!("Config path: {}", path.display());
    let config_str = match fs::read_to_string(path) {
        Ok(contents) => contents,
        // Only a genuinely absent file is "not found" — the daemon turns that
        // into an empty config. Any other read failure (permissions, I/O, the
        // path is a directory, ...) must NOT be mistaken for absence, or the
        // daemon would overwrite an existing-but-unreadable config with an
        // empty one. Surface those as a distinct error instead.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ConfigParseError::ConfigNotFound);
        }
        Err(e) => {
            log::error!("Error reading config {}: {e}", path.display());
            return Err(ConfigParseError::ReadError(e.to_string()));
        }
    };
    serde_json::from_str::<LocalConfig>(&config_str).map_err(|e| {
        log::error!("Error deserialize: {e}");
        ConfigParseError::SerializeError
    })
}

pub fn create_empty_config() -> Result<(), ConfigParseError> {
    create_empty_config_at(&config_file_path())
}

/// Persist a fresh empty config at `path`. Separate from [`create_empty_config`]
/// so the daemon can honor a `--config <PATH>` override (and so it is testable
/// against a temp path).
pub fn create_empty_config_at(path: &Path) -> Result<(), ConfigParseError> {
    let empty_config = LocalConfig {
        vpn_name: String::new(),
        vpn_hosts: Vec::new(),
        enabled: default_enabled(),
        vpn_backend: VpnBackend::default(),
        openvpn: OpenVpnConfig::default(),
    };
    save_config_to(path, &empty_config)
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

/// Write `contents` to `path` atomically: write a unique temp sibling, fsync
/// it, then rename it over `path`. A crash mid-write leaves either the old
/// file or the complete new file behind — never a truncated file.
///
/// The temp name is a hidden, per-process-unique sibling
/// (`.splitway.<pid>.<n>.tmp`) opened with `O_EXCL`, so it can neither alias nor
/// truncate an existing target whose name is caller-controlled — e.g. an
/// `/etc/resolver/<domain>` file, where a domain like `foo.tmp` would otherwise
/// collide with a `with_extension("tmp")` temp path.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "config path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    // Create a temp file we exclusively own (O_EXCL), so cleaning it up on
    // failure can never delete a pre-existing file at that path.
    let (mut file, tmp) = create_temp_file(dir)?;
    let write_result = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Best-effort: fsync the directory so the rename itself is durable.
    if let Ok(dir_file) = File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Create and open a fresh, exclusively-owned (`O_EXCL`) temp file under `dir`,
/// retrying on the rare name collision (e.g. a leftover temp from a previous
/// run that reused this pid) with a new name rather than failing. Returns the
/// open file and the path it created, so the caller cleans up only a temp it
/// created — never a pre-existing file.
fn create_temp_file(dir: &Path) -> io::Result<(File, PathBuf)> {
    for _ in 0..1000 {
        let tmp = dir.join(temp_file_name());
        match File::create_new(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temp file after many attempts",
    ))
}

/// A hidden temp filename unique within this process, used as the atomic-write
/// sibling. Dot-prefixed and `.tmp`-suffixed so it is never mistaken for a real
/// target; the pid + monotonic counter make repeated or concurrent writes
/// collision-free.
fn temp_file_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".splitway.{}.{n}.tmp", std::process::id())
}

pub fn config_file_path() -> PathBuf {
    config_folder_path().join("config.json")
}

fn config_folder_path() -> PathBuf {
    // Resolve without panicking — this runs inside a long-lived daemon that
    // may be a systemd service where HOME is not guaranteed. Prefer
    // $XDG_CONFIG_HOME, then $HOME/.config, then a root-service fallback.
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("splitway");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join("splitway");
        }
    }
    log::warn!("neither XDG_CONFIG_HOME nor HOME is set; falling back to /root/.config/splitway");
    PathBuf::from("/root/.config/splitway")
}

fn default_enabled() -> bool {
    true
}

/// Which Linux VPN detector the daemon uses. macOS/Windows have a single
/// detector and ignore this field. Defaults to [`VpnBackend::NetworkManager`]
/// so configs written before this field existed keep their behavior.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VpnBackend {
    /// Detect via NetworkManager over D-Bus (the original Linux detector).
    #[default]
    NetworkManager,
    /// Detect a standalone OpenVPN connection via its management interface.
    /// `rename` pins the config token to `openvpn` (kebab-case of `OpenVpn`
    /// would be `open-vpn`); the Rust name matches `OpenVpnConfig` /
    /// `OpenVpnDetector` for consistent casing across the codebase.
    #[serde(rename = "openvpn")]
    OpenVpn,
}

/// Connection settings for a standalone OpenVPN's management interface. Used
/// only when [`LocalConfig::vpn_backend`] is [`VpnBackend::OpenVpn`].
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenVpnConfig {
    /// The management interface address: either a TCP endpoint `host:port`
    /// (matching `management 127.0.0.1 7505` in `openvpn.conf`) or a path to a
    /// unix socket (matching `management /run/openvpn/mgmt.sock unix`). A value
    /// containing `/` is treated as a unix socket path, otherwise as `host:port`.
    #[serde(default)]
    pub management: String,
    /// Optional path to a file whose first line is the management password,
    /// for a password-protected management interface. `None` = no password.
    #[serde(default)]
    pub management_password_file: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    pub vpn_name: String,
    pub vpn_hosts: Vec<String>,
    /// Whether the daemon applies rules. Defaults to `true` so configs
    /// written before this field existed keep applying as before.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Which VPN detector to use on Linux. `#[serde(default)]` keeps pre-3c
    /// configs (without this field) selecting NetworkManager.
    #[serde(default)]
    pub vpn_backend: VpnBackend,
    /// Standalone-OpenVPN management connection. `#[serde(default)]` keeps
    /// pre-3c configs parsing; ignored unless `vpn_backend = openvpn`.
    #[serde(default)]
    pub openvpn: OpenVpnConfig,
}

#[derive(Debug)]
pub enum ConfigParseError {
    ConfigNotFound,
    /// The config exists but could not be read (permissions, I/O, ...). Kept
    /// distinct from `ConfigNotFound` so callers never overwrite it with an
    /// empty config.
    ReadError(String),
    SerializeError,
    WriteError(String),
}

impl Display for ConfigParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigParseError::ConfigNotFound => write!(f, "Config file not found"),
            ConfigParseError::ReadError(e) => write!(f, "Error reading config: {e}"),
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
        // Start from a clean directory so assertions (e.g. "no temp file left
        // behind") reflect only this run, not leftovers from a crashed run or a
        // reused pid.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[test]
    fn absent_config_is_not_found_but_unreadable_is_read_error() {
        // A genuinely missing file -> ConfigNotFound (daemon creates empty).
        let mut missing = std::env::temp_dir();
        missing.push(format!("splitway-absent-{}.json", std::process::id()));
        let _ = fs::remove_file(&missing);
        assert!(matches!(
            load_config_from(&missing),
            Err(ConfigParseError::ConfigNotFound)
        ));

        // An existing-but-unreadable path (here a directory) -> ReadError, so
        // the daemon never mistakes it for absence and overwrites it.
        let mut dir = std::env::temp_dir();
        dir.push(format!("splitway-readerr-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            load_config_from(&dir),
            Err(ConfigParseError::ReadError(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_config_serde_round_trip() {
        let config = LocalConfig {
            vpn_name: "wg0".to_string(),
            vpn_hosts: vec!["example.com".to_string(), "internal.corp".to_string()],
            enabled: false,
            vpn_backend: VpnBackend::OpenVpn,
            openvpn: OpenVpnConfig {
                management: "127.0.0.1:7505".to_string(),
                management_password_file: Some("/etc/splitway/mgmt.pass".to_string()),
            },
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
    fn vpn_backend_defaults_to_network_manager_when_absent() {
        // Pre-3c configs (no vpn_backend / openvpn fields) must still parse and
        // keep selecting the NetworkManager detector — no behavior change.
        let json = r#"{"vpn_name":"tun0","vpn_hosts":["a.com"],"enabled":true}"#;
        let parsed: LocalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.vpn_backend, VpnBackend::NetworkManager);
        assert_eq!(parsed.openvpn, OpenVpnConfig::default());
        assert!(parsed.openvpn.management.is_empty());
        assert!(parsed.openvpn.management_password_file.is_none());
    }

    #[test]
    fn vpn_backend_parses_kebab_case_values() {
        // The serialized form uses kebab-case tokens.
        let nm = r#"{"vpn_name":"","vpn_hosts":[],"vpn_backend":"network-manager"}"#;
        assert_eq!(
            serde_json::from_str::<LocalConfig>(nm).unwrap().vpn_backend,
            VpnBackend::NetworkManager
        );
        let ovpn = r#"{"vpn_name":"tun0","vpn_hosts":[],"vpn_backend":"openvpn","openvpn":{"management":"127.0.0.1:7505"}}"#;
        let parsed: LocalConfig = serde_json::from_str(ovpn).unwrap();
        assert_eq!(parsed.vpn_backend, VpnBackend::OpenVpn);
        assert_eq!(parsed.openvpn.management, "127.0.0.1:7505");
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
        let leftover_temp = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with(".splitway."));
        assert!(!leftover_temp, "atomic_write left a temp file behind");
    }

    #[test]
    fn save_config_to_round_trips() {
        let path = temp_config("save-round-trip");
        let config = LocalConfig {
            vpn_name: "tun0".to_string(),
            vpn_hosts: vec!["corp.example".to_string()],
            enabled: true,
            vpn_backend: VpnBackend::default(),
            openvpn: OpenVpnConfig::default(),
        };
        save_config_to(&path, &config).unwrap();
        let loaded = load_config_from(&path).unwrap();
        assert_eq!(loaded, config);
    }
}
