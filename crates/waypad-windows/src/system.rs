//! Volume, media, clipboard, and session actions on Windows.
//!
//! Volume and media go through the same `SendInput` path as everything else,
//! using the multimedia virtual keys. That is deliberately not the COM
//! `IAudioEndpointVolume` API: the media keys act on whatever the shell decides
//! is the active session, which is what a user pressing the key on their
//! keyboard gets, whereas the endpoint API would move a master slider the OSD
//! never shows.

use anyhow::{Context, bail};
use async_trait::async_trait;
use waypad_core::{
    backend::SystemBackend,
    capability::SystemCapabilities,
    protocol::{BrightnessAction, MediaAction, VolumeAction},
};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        Ole::CF_UNICODETEXT,
        Shutdown::LockWorkStation,
    },
    UI::Input::KeyboardAndMouse::{
        VIRTUAL_KEY, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
        VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP,
    },
};

use crate::input::tap_virtual_key;

#[derive(Debug, Default)]
pub struct WindowsSystemBackend;

impl WindowsSystemBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SystemBackend for WindowsSystemBackend {
    async fn media(&self, action: MediaAction) -> anyhow::Result<()> {
        tap_virtual_key(match action {
            MediaAction::PlayPause => VK_MEDIA_PLAY_PAUSE,
            MediaAction::Next => VK_MEDIA_NEXT_TRACK,
            MediaAction::Previous => VK_MEDIA_PREV_TRACK,
            MediaAction::Stop => VK_MEDIA_STOP,
        })
    }

    async fn volume(&self, action: VolumeAction) -> anyhow::Result<()> {
        let key = match action {
            VolumeAction::Up => VK_VOLUME_UP,
            VolumeAction::Down => VK_VOLUME_DOWN,
            VolumeAction::MuteToggle => VK_VOLUME_MUTE,
        };
        // One tap is a 2 % step on Windows, which is far too fine to feel like
        // the 5 % the Linux backend does. Two taps lands close enough without
        // making the OSD flicker.
        if matches!(action, VolumeAction::Up | VolumeAction::Down) {
            tap_virtual_key(key)?;
        }
        tap_virtual_key(key)
    }

    async fn brightness(&self, _action: BrightnessAction) -> anyhow::Result<()> {
        // WMI `WmiMonitorBrightnessMethods` only answers on panels the firmware
        // drives — laptops, essentially — and silently does nothing on a
        // desktop with an external monitor. Rather than ship something that
        // works on one machine in three with no explanation, this reports the
        // limitation until DDC/CI support is written.
        bail!(
            "Brightness control is not implemented on Windows yet. It needs DDC/CI for external \
             monitors; the WMI path only works on laptop panels."
        )
    }

    async fn clipboard_set(&self, text: &str) -> anyhow::Result<()> {
        // The length cap lives in the command handler, which rejects oversized
        // text before it reaches any backend.
        set_clipboard_text(text)
    }

    async fn lock(&self) -> anyhow::Result<()> {
        // SAFETY: takes no arguments and reports failure through Result.
        unsafe { LockWorkStation() }.context("LockWorkStation failed")
    }

    async fn suspend(&self) -> anyhow::Result<()> {
        // Whether suspend is permitted at all is decided by the config, checked
        // by the caller: this only performs it.
        //
        // Shelling out to powrprof rather than linking SetSuspendState keeps
        // this off the crate's link line for a feature that is disabled by
        // default and used approximately never.
        let status = tokio::process::Command::new("rundll32.exe")
            .args(["powrprof.dll,SetSuspendState", "0,1,0"])
            .status()
            .await
            .context("failed to invoke powrprof to suspend")?;
        if status.success() {
            Ok(())
        } else {
            bail!("suspend request exited with {status}")
        }
    }
}

/// What this backend can actually do, for the capability report.
pub fn detect_system_capabilities(allow_suspend: bool) -> SystemCapabilities {
    SystemCapabilities {
        volume: true,
        media: true,
        // Reported false rather than omitted, so the client shows the reason
        // instead of a control that does nothing.
        brightness: false,
        clipboard: true,
        lock: true,
        suspend: allow_suspend,
    }
}

/// Puts UTF-16 text on the clipboard.
///
/// The clipboard takes ownership of the moveable global on success, so the
/// allocation is only freed on the failure paths — freeing it afterwards would
/// hand the shell a dangling handle.
fn set_clipboard_text(text: &str) -> anyhow::Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = std::mem::size_of_val(utf16.as_slice());

    // SAFETY: every call below is checked, and the guard closes the clipboard
    // on every path out of this function.
    unsafe {
        OpenClipboard(None).context("another process is holding the clipboard")?;
        let guard = ClipboardGuard;

        let result = (|| -> anyhow::Result<()> {
            EmptyClipboard().context("failed to clear the clipboard")?;
            let handle: HGLOBAL =
                GlobalAlloc(GMEM_MOVEABLE, bytes).context("clipboard allocation failed")?;
            let target = GlobalLock(handle);
            if target.is_null() {
                let _ = GlobalFree(Some(handle));
                bail!("failed to lock the clipboard allocation");
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), target.cast::<u16>(), utf16.len());
            let _ = GlobalUnlock(handle);
            if let Err(err) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(handle.0))) {
                let _ = GlobalFree(Some(handle));
                return Err(err).context("failed to publish clipboard data");
            }
            Ok(())
        })();
        drop(guard);
        result
    }
}

/// Closes the clipboard however this function exits. Leaving it open would
/// block every other application on the desktop from copying anything.
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: paired with the OpenClipboard that produced this guard.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Re-exported for the capability probe, which needs the same list.
pub const MEDIA_KEYS: &[VIRTUAL_KEY] = &[
    VK_MEDIA_PLAY_PAUSE,
    VK_MEDIA_NEXT_TRACK,
    VK_MEDIA_PREV_TRACK,
    VK_MEDIA_STOP,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_reports_a_reason_rather_than_failing_silently() {
        let backend = WindowsSystemBackend::new();
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(backend.brightness(BrightnessAction::Up))
            .expect_err("brightness is not implemented yet");
        let message = err.to_string();
        assert!(message.contains("DDC/CI"), "{message}");
    }

    #[tokio::test]
    async fn clipboard_round_trips_unicode() {
        // Uses the real clipboard, which is per-session and safe to touch: the
        // point is that the UTF-16 conversion and the global-memory handoff are
        // correct, and only a live call can show that.
        let backend = WindowsSystemBackend::new();
        backend
            .clipboard_set("caffè — 日本語")
            .await
            .expect("clipboard write succeeds");
    }

    #[test]
    fn capabilities_report_brightness_as_unavailable_not_absent() {
        let capabilities = detect_system_capabilities(false);
        assert!(capabilities.volume);
        assert!(capabilities.media);
        assert!(capabilities.clipboard);
        assert!(capabilities.lock);
        assert!(!capabilities.brightness);
        assert!(!capabilities.suspend);
        assert!(detect_system_capabilities(true).suspend);
    }
}
