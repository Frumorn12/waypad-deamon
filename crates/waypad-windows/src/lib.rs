//! Windows backends for the Waypad host.
//!
//! Implements the traits in `waypad_core::backend` with SendInput, DXGI
//! Desktop Duplication, Media Foundation, and WASAPI.
//!
//! Gated on Windows so the crate can stay a workspace member everywhere:
//! elsewhere it compiles to an empty library instead of dragging the Win32
//! bindings into a build that has no use for them.

#![cfg(windows)]

pub mod audio;
pub mod autostart;
pub mod capture;
pub mod encoder;
pub mod host;
pub mod input;
pub mod nv12;
pub mod screen;
pub mod system;
pub mod tray;

/// Whether this session has a desktop the capture tests can use.
///
/// CI runners vary: some expose a virtual display and enumerate outputs, some
/// do not. Tests that need one check here and report a skip rather than failing
/// on a machine that was never going to be able to run them — while still
/// asserting in full wherever a desktop does exist.
#[cfg(test)]
pub(crate) fn test_desktop_available() -> bool {
    capture::enumerate_outputs()
        .map(|outputs| !outputs.is_empty())
        .unwrap_or(false)
}

/// Skips the calling test when there is no desktop, saying so out loud.
#[cfg(test)]
macro_rules! skip_without_desktop {
    () => {
        if !$crate::test_desktop_available() {
            eprintln!("skipped: this session has no desktop to capture");
            return;
        }
    };
}

#[cfg(test)]
pub(crate) use skip_without_desktop;
