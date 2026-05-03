use crate::{
    config::Config,
    crypto::{HostIdentity, random_pairing_code, random_token, sha256_hex},
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub created_at: u64,
    pub last_seen_at: Option<u64>,
    pub revoked: bool,
    pub app_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrustedDevices {
    pub devices: Vec<TrustedDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingCodeFile {
    code_hash: String,
    created_at: u64,
    expires_at: u64,
}

#[derive(Debug, Clone)]
pub struct PairingCode {
    pub code: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
pub struct StatePaths {
    pub state_dir: PathBuf,
    pub host_identity: PathBuf,
    pub trusted_devices: PathBuf,
    pub pairing_code: PathBuf,
    pub portal_restore_token: PathBuf,
}

impl StatePaths {
    pub fn new(config: &Config) -> Self {
        let state_dir = config.state_dir.clone();
        Self {
            host_identity: state_dir.join("host_identity.json"),
            trusted_devices: state_dir.join("trusted_devices.json"),
            pairing_code: state_dir.join("pairing_code.json"),
            portal_restore_token: state_dir.join("portal_restore_token.json"),
            state_dir,
        }
    }
}

pub fn ensure_state_dir(paths: &StatePaths) -> io::Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    set_private_dir_permissions(&paths.state_dir)?;
    Ok(())
}

pub fn load_or_create_identity(paths: &StatePaths) -> anyhow::Result<HostIdentity> {
    ensure_state_dir(paths)?;
    if paths.host_identity.exists() {
        let raw = fs::read_to_string(&paths.host_identity)?;
        return Ok(serde_json::from_str(&raw)?);
    }
    let identity = HostIdentity::generate()?;
    write_private_json(&paths.host_identity, &identity)?;
    Ok(identity)
}

pub fn rotate_identity(paths: &StatePaths) -> anyhow::Result<HostIdentity> {
    ensure_state_dir(paths)?;
    let identity = HostIdentity::generate()?;
    write_private_json(&paths.host_identity, &identity)?;
    Ok(identity)
}

pub fn load_trusted_devices(paths: &StatePaths) -> anyhow::Result<TrustedDevices> {
    ensure_state_dir(paths)?;
    if !paths.trusted_devices.exists() {
        return Ok(TrustedDevices::default());
    }
    let raw = fs::read_to_string(&paths.trusted_devices)?;
    Ok(serde_json::from_str(&raw)?)
}

pub fn save_trusted_devices(paths: &StatePaths, devices: &TrustedDevices) -> anyhow::Result<()> {
    write_private_json(&paths.trusted_devices, devices)?;
    Ok(())
}

impl TrustedDevices {
    pub fn pair_device(
        &mut self,
        name: String,
        app_version: Option<String>,
    ) -> anyhow::Result<(TrustedDevice, String)> {
        let token = random_token()?;
        let now = now_unix();
        let device = TrustedDevice {
            id: Uuid::new_v4().to_string(),
            name,
            token_hash: sha256_hex(token.as_bytes()),
            created_at: now,
            last_seen_at: Some(now),
            revoked: false,
            app_version,
        };
        self.devices.retain(|d| d.id != device.id);
        self.devices.push(device.clone());
        Ok((device, token))
    }

    pub fn authenticate(&mut self, device_id: &str, token: &str) -> Option<TrustedDevice> {
        let token_hash = sha256_hex(token.as_bytes());
        let now = now_unix();
        let device = self
            .devices
            .iter_mut()
            .find(|d| d.id == device_id && !d.revoked && d.token_hash == token_hash)?;
        device.last_seen_at = Some(now);
        Some(device.clone())
    }

    pub fn revoke(&mut self, device_id: &str) -> bool {
        if let Some(device) = self.devices.iter_mut().find(|d| d.id == device_id) {
            device.revoked = true;
            return true;
        }
        false
    }
}

pub fn create_pairing_code(config: &Config, paths: &StatePaths) -> anyhow::Result<PairingCode> {
    ensure_state_dir(paths)?;
    let code = random_pairing_code()?;
    let now = now_unix();
    let expires_at = now + config.pairing_code_ttl_seconds;
    let file = PairingCodeFile {
        code_hash: sha256_hex(code.as_bytes()),
        created_at: now,
        expires_at,
    };
    write_private_json(&paths.pairing_code, &file)?;
    Ok(PairingCode { code, expires_at })
}

pub fn validate_pairing_code(paths: &StatePaths, code: &str) -> anyhow::Result<bool> {
    if !paths.pairing_code.exists() {
        return Ok(false);
    }
    let raw = fs::read_to_string(&paths.pairing_code)?;
    let file: PairingCodeFile = serde_json::from_str(&raw)?;
    let valid = file.expires_at >= now_unix() && file.code_hash == sha256_hex(code.as_bytes());
    if valid {
        let _ = fs::remove_file(&paths.pairing_code);
    }
    Ok(valid)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_private_dir_permissions(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let raw = serde_json::to_vec_pretty(value).expect("state value serializes");
    fs::write(&tmp, [raw.as_slice(), b"\n"].concat())?;
    set_private_file_permissions(&tmp)?;
    fs::rename(tmp, path)?;
    set_private_file_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn save_portal_restore_token(paths: &StatePaths, token: &str) -> anyhow::Result<()> {
    ensure_state_dir(paths)?;
    let path = &paths.portal_restore_token;
    let data = serde_json::json!({ "restore_token": token, "saved_at": now_unix() });
    fs::write(path, serde_json::to_string(&data)?)?;
    set_private_file_permissions(path)?;
    Ok(())
}

pub fn load_portal_restore_token(paths: &StatePaths) -> Option<String> {
    let path = &paths.portal_restore_token;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("restore_token")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_auth_uses_hash_and_revocation() {
        let mut store = TrustedDevices::default();
        let (device, token) = store.pair_device("phone".into(), None).unwrap();
        assert!(store.authenticate(&device.id, &token).is_some());
        assert!(store.authenticate(&device.id, "wrong").is_none());
        assert!(store.revoke(&device.id));
        assert!(store.authenticate(&device.id, &token).is_none());
    }
}
