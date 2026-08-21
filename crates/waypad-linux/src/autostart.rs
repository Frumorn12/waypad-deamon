//! Registering the daemon to start with the graphical session.
//!
//! A `systemd --user` unit rather than an XDG autostart entry, because Waypad
//! needs the session's Wayland and portal environment and the user manager is
//! what exports it. The unit is written on demand if it is not already there,
//! so enabling the toggle in the panel works on a host where nobody ran the
//! install steps from the README.

use anyhow::{Context, bail};
use std::{fs, path::PathBuf, process::Command};

const UNIT_NAME: &str = "waypad-daemon.service";

fn unit_path() -> PathBuf {
    waypad_core::config::config_home()
        .join("systemd")
        .join("user")
        .join(UNIT_NAME)
}

/// The unit written when none is installed.
///
/// Bound to `graphical-session.target` on purpose: started any earlier, the
/// user manager has not yet been told about `WAYLAND_DISPLAY` and the daemon
/// comes up with no session to control.
fn unit_contents(exec: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Waypad remote control daemon\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} serve\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n"
    )
}

pub fn is_enabled() -> anyhow::Result<bool> {
    if !crate::platform::command_exists("systemctl") {
        bail!("systemctl is not installed, so login start cannot be managed");
    }
    let output = Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT_NAME])
        .output()
        .context("could not ask systemd whether Waypad starts at login")?;
    // A unit that is absent or disabled exits non-zero; only "enabled" counts,
    // and "linked"/"static" are not states this writes, so they read as off.
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "enabled")
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    if !crate::platform::command_exists("systemctl") {
        bail!("systemctl is not installed, so login start cannot be managed");
    }
    if enabled {
        install_unit()?;
        run(&["--user", "daemon-reload"])?;
        run(&["--user", "enable", UNIT_NAME])
    } else {
        // The unit file is left in place. Disabling is reversible; deleting
        // something the user may have edited is not.
        run(&["--user", "disable", UNIT_NAME])
    }
}

fn install_unit() -> anyhow::Result<()> {
    let path = unit_path();
    if path.exists() {
        // Never overwritten: a host that was set up from the README may have a
        // unit with local edits in it.
        return Ok(());
    }
    let exec = std::env::current_exe()
        .context("could not determine the Waypad executable path")?
        .display()
        .to_string();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(&path, unit_contents(&exec))
        .with_context(|| format!("could not write {}", path.display()))
}

fn run(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .context("could not run systemctl")?;
    if status.success() {
        Ok(())
    } else {
        bail!("systemctl {} exited with {status}", args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_waits_for_the_graphical_session() {
        let unit = unit_contents("/usr/bin/waypad-daemon");
        // Started before the session target, the user manager has not exported
        // WAYLAND_DISPLAY yet and the daemon finds nothing to control.
        assert!(unit.contains("After=graphical-session.target"), "{unit}");
        assert!(unit.contains("WantedBy=graphical-session.target"), "{unit}");
        assert!(
            unit.contains("ExecStart=/usr/bin/waypad-daemon serve"),
            "{unit}"
        );
    }

    #[test]
    fn the_unit_lives_under_the_user_config_directory() {
        let path = unit_path();
        assert!(
            path.ends_with("systemd/user/waypad-daemon.service"),
            "{path:?}"
        );
    }
}
