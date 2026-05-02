use serde::{Deserialize, Serialize};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub bind_address: String,
    pub control_port: u16,
    pub discovery_port: u16,
    pub require_private_lan: bool,
    pub state_dir: PathBuf,
    pub pairing_code_ttl_seconds: u64,
    pub max_pair_attempts_per_minute: u32,
    pub allow_suspend: bool,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            control_port: 47771,
            discovery_port: 47770,
            require_private_lan: true,
            state_dir: default_state_dir(),
            pairing_code_ttl_seconds: 300,
            max_pair_attempts_per_minute: 5,
            allow_suspend: false,
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Ok(path) = env::var("WAYPAD_CONFIG") {
            return PathBuf::from(path);
        }
        config_home().join("waypad-daemon").join("config.json")
    }

    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)?;
        let mut config: Self = serde_json::from_str(&raw)?;
        if config.state_dir.as_os_str().is_empty() {
            config.state_dir = default_state_dir();
        }
        Ok(config)
    }

    pub fn write_sample(path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text =
            serde_json::to_string_pretty(&Self::default()).expect("default config serializes");
        fs::write(path, format!("{text}\n"))
    }
}

pub fn config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

pub fn default_state_dir() -> PathBuf {
    env::var_os("WAYPAD_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("waypad-daemon")))
        .unwrap_or_else(|| home_dir().join(".local/state/waypad-daemon"))
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_lan_only() {
        let config = Config::default();
        assert!(config.require_private_lan);
        assert_eq!(config.control_port, 47771);
        assert!(!config.allow_suspend);
    }
}
