use serde::{Deserialize, Serialize};
use std::{env, ffi::OsString, path::PathBuf, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_type: String,
    pub wayland_display: Option<String>,
    pub x11_display: Option<String>,
    pub current_desktop: Option<String>,
    pub desktop_session: Option<String>,
    pub hyprland_instance_signature: Option<String>,
    pub compositor_hint: String,
    pub hyprctl_version: Option<String>,
}

pub fn detect_session() -> SessionInfo {
    let session_type = env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| {
        if env::var_os("WAYLAND_DISPLAY").is_some() {
            "wayland".into()
        } else if env::var_os("DISPLAY").is_some() {
            "x11".into()
        } else {
            "unknown".into()
        }
    });
    let current_desktop = env::var("XDG_CURRENT_DESKTOP").ok();
    let desktop_session = env::var("DESKTOP_SESSION").ok();
    let hyprland_instance_signature = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    let compositor_hint = if hyprland_instance_signature.is_some()
        || current_desktop
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("hyprland")
    {
        "hyprland".into()
    } else if session_type == "wayland" {
        "wayland".into()
    } else {
        "unknown".into()
    };

    SessionInfo {
        session_type,
        wayland_display: env::var("WAYLAND_DISPLAY").ok(),
        x11_display: env::var("DISPLAY").ok(),
        current_desktop,
        desktop_session,
        hyprland_instance_signature,
        compositor_hint,
        hyprctl_version: command_output("hyprctl", &["version"]),
    }
}

pub fn command_exists(name: &str) -> bool {
    if name.contains('/') {
        return PathBuf::from(name).is_file();
    }
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    if !command_exists(program) {
        return None;
    }
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

pub fn env_os(name: &str) -> Option<OsString> {
    env::var_os(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_command_is_false() {
        assert!(!command_exists("definitely-not-a-real-waypad-command"));
    }
}
