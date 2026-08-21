//! Registering the daemon to start when the user logs in.
//!
//! The per-user `Run` key, deliberately: it needs no administrator, it runs in
//! the user's own interactive session — which is the only place input injection
//! and desktop duplication work at all — and uninstalling it is one value
//! deletion rather than a service to unregister.

use anyhow::{Context, bail};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{PCWSTR, w};

/// The value name under `Run`. Stable, because changing it would leave the old
/// one behind and start the daemon twice.
const VALUE_NAME: PCWSTR = w!("Waypad");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

/// The command the Run key should hold: this executable, serving.
fn autostart_command() -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("could not determine the Waypad executable path")?;
    // Quoted because Program Files has a space in it, and an unquoted path with
    // a space is read as a command plus arguments.
    Ok(format!("\"{}\" serve", exe.display()))
}

fn to_utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

struct RunKey(HKEY);

impl RunKey {
    fn open(write: bool) -> anyhow::Result<Self> {
        let mut key = HKEY::default();
        let access = if write {
            KEY_READ | KEY_WRITE
        } else {
            KEY_READ
        };
        // SAFETY: the key is closed by Drop; the name is a static wide string.
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key) }
            .ok()
            .context("could not open the per-user Run key")?;
        Ok(Self(key))
    }
}

impl Drop for RunKey {
    fn drop(&mut self) {
        // SAFETY: paired with the RegOpenKeyExW that produced this handle.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

pub fn is_enabled() -> anyhow::Result<bool> {
    let key = RunKey::open(false)?;
    let mut size = 0u32;
    // SAFETY: querying with a null buffer only reports the size, which is how
    // presence is tested without allocating.
    let result = unsafe { RegQueryValueExW(key.0, VALUE_NAME, None, None, None, Some(&mut size)) };
    Ok(result.is_ok())
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    let key = RunKey::open(true)?;
    if !enabled {
        // SAFETY: deleting an absent value is reported as an error and treated
        // as success below, since the desired state is already reached.
        let result = unsafe { RegDeleteValueW(key.0, VALUE_NAME) };
        if result.is_err() && is_enabled().unwrap_or(false) {
            bail!("could not remove Waypad from the login items");
        }
        return Ok(());
    }

    let command = autostart_command()?;
    let wide = to_utf16(&command);
    // SAFETY: the byte view borrows `wide`, which outlives the call.
    let bytes = unsafe {
        std::slice::from_raw_parts(wide.as_ptr() as *const u8, std::mem::size_of_val(&wide[..]))
    };
    unsafe { RegSetValueExW(key.0, VALUE_NAME, None, REG_SZ, Some(bytes)) }
        .ok()
        .context("could not add Waypad to the login items")?;
    Ok(())
}

/// Same as [`set_enabled`], named for the caller that has no `PlatformHost` to
/// hand: the tray menu runs on its own Win32 thread and owns nothing.
pub fn set_autostart_from_tray(enabled: bool) -> anyhow::Result<()> {
    set_enabled(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_quotes_the_path_and_asks_for_serve() {
        let command = autostart_command().unwrap();
        assert!(command.starts_with('"'), "{command}");
        assert!(command.ends_with("\" serve"), "{command}");
        // Program Files has a space in it, and an unquoted path with a space
        // starts a different program with an argument.
        assert!(command.matches('"').count() == 2, "{command}");
    }

    #[test]
    fn reading_the_current_state_never_fails_on_a_normal_account() {
        // The Run key exists on every Windows install; being unable to read it
        // would mean the panel could not show the toggle at all.
        assert!(is_enabled().is_ok());
    }

    #[test]
    fn enabling_then_disabling_returns_to_the_original_state() {
        let original = is_enabled().unwrap();
        set_enabled(true).unwrap();
        assert!(is_enabled().unwrap());
        set_enabled(false).unwrap();
        assert!(!is_enabled().unwrap());
        // Disabling twice is not an error: the desired state is already there.
        set_enabled(false).unwrap();
        set_enabled(original).unwrap();
        assert_eq!(is_enabled().unwrap(), original);
    }
}
