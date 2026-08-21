//! The notification-area icon.
//!
//! Written directly against `Shell_NotifyIcon` rather than through a tray
//! crate: the usual ones pull GTK in on Linux, which would add a build
//! prerequisite and break the clean cross-compile for a feature Linux does not
//! use — it has a terminal and a systemd unit already.
//!
//! A tray icon needs a window to receive its callbacks, and a window needs a
//! message loop on the thread that created it. Both live on a thread of their
//! own here, so the async runtime never has to host a Win32 pump.

use anyhow::{Context, bail};
use std::sync::OnceLock;
use tracing::{debug, info, warn};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
            DispatchMessageW, GetCursorPos, GetMessageW, HWND_MESSAGE, IDI_APPLICATION, LoadIconW,
            MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, PostQuitMessage,
            RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TrackPopupMenu,
            TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP,
            WNDCLASSW, WS_EX_NOACTIVATE, WS_OVERLAPPED,
        },
    },
};
use windows::core::{PCWSTR, w};

/// The callback the icon sends its mouse events to. Anything from `WM_APP` up
/// is reserved for the application, so this cannot collide with a system message.
const WM_TRAY: u32 = WM_APP + 1;

/// The first icon a resource file defines gets id 1, which is what the build
/// script hands to `winresource`.
const ICON_RESOURCE_ID: usize = 1;

const ID_OPEN: usize = 1;
const ID_COPY: usize = 2;
const ID_AUTOSTART: usize = 3;
const ID_QUIT: usize = 4;

/// What the tray needs to act on. One tray per process, so a static is honest
/// about the lifetime rather than pretending there could be several.
struct TrayState {
    panel_url: String,
}

static STATE: OnceLock<TrayState> = OnceLock::new();

/// Starts the tray on its own thread and returns once the icon is visible.
///
/// Failure is not fatal to the daemon: a host with no notification area still
/// serves phones perfectly well, and the panel URL is printed either way.
pub fn spawn(panel_url: String) -> anyhow::Result<()> {
    if STATE.set(TrayState { panel_url }).is_err() {
        bail!("the tray is already running");
    }
    std::thread::Builder::new()
        .name("waypad-tray".into())
        .spawn(|| {
            if let Err(err) = run() {
                warn!(%err, "the tray icon could not be created; the panel URL still works");
            }
        })
        .context("could not start the tray thread")?;
    Ok(())
}

fn run() -> anyhow::Result<()> {
    // SAFETY: the window class, window and icon are created and destroyed on
    // this thread, which is also the one pumping their messages.
    unsafe {
        let instance = GetModuleHandleW(None).context("could not get the module handle")?;
        let class_name = w!("WaypadTrayWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            bail!("could not register the tray window class");
        }

        // Message-only: it has no presence on screen and never appears in the
        // task bar or in alt-tab, which is what a background daemon wants.
        let window = CreateWindowExW(
            WS_EX_NOACTIVATE,
            class_name,
            w!("Waypad"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )
        .context("could not create the tray window")?;

        let mut icon_data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            // The executable's own icon when it has one, so the tray matches
            // what Explorer shows; the stock application icon otherwise.
            //
            // `without_provenance` rather than a cast: MAKEINTRESOURCE encodes
            // a small integer resource id in the pointer field, and this is the
            // way to say "an integer that is not an address" without inventing
            // provenance the value never had.
            hIcon: LoadIconW(
                Some(instance.into()),
                PCWSTR(std::ptr::without_provenance(ICON_RESOURCE_ID)),
            )
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .context("could not load a tray icon")?,
            ..Default::default()
        };
        set_tip(&mut icon_data, "Waypad — click to open the control panel");
        if !Shell_NotifyIconW(NIM_ADD, &icon_data).as_bool() {
            bail!("Windows refused to add the tray icon");
        }
        info!("tray icon added");

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data);
        Ok(())
    }
}

/// Copies a tooltip into the fixed-size field the struct carries.
fn set_tip(data: &mut NOTIFYICONDATAW, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // Truncated rather than refused: the tooltip is decoration, and the field
    // has to stay null terminated whatever happens.
    let room = data.szTip.len() - 1;
    let take = wide.len().min(room);
    data.szTip[..take].copy_from_slice(&wide[..take]);
    data.szTip[take] = 0;
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => open_panel(),
                WM_RBUTTONUP => unsafe { show_menu(window) },
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                ID_OPEN => open_panel(),
                ID_COPY => copy_panel_url(),
                ID_AUTOSTART => toggle_autostart(),
                ID_QUIT => {
                    info!("quitting from the tray menu");
                    // The daemon has no other windows and nothing to flush; the
                    // control channel drops as the process goes, which is what a
                    // client sees from any other kind of shutdown too.
                    std::process::exit(0);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends the loop this window's thread is pumping.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn show_menu(window: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let autostart = crate::autostart::is_enabled().unwrap_or(false);
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Open control panel"));
        let _ = AppendMenuW(menu, MF_STRING, ID_COPY, w!("Copy panel link"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING | if autostart { MF_CHECKED } else { MF_UNCHECKED },
            ID_AUTOSTART,
            w!("Start with Windows"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit Waypad"));

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        // Documented requirement: without this the menu does not close when the
        // user clicks elsewhere, and sits over the desktop until clicked again.
        let _ = SetForegroundWindow(window);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            cursor.x,
            cursor.y,
            None,
            window,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}

fn panel_url() -> Option<&'static str> {
    STATE.get().map(|state| state.panel_url.as_str())
}

fn open_panel() {
    let Some(url) = panel_url() else { return };
    if let Err(err) = open_in_browser(url) {
        warn!(%err, "could not open the control panel from the tray");
    }
}

fn open_in_browser(url: &str) -> anyhow::Result<()> {
    // The empty string is the window title `start` takes first; without it a
    // quoted URL is read as the title and nothing opens.
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
        .context("could not launch a browser")
}

fn copy_panel_url() {
    let Some(url) = panel_url() else { return };
    match crate::system::set_clipboard_text(url) {
        Ok(()) => debug!("panel link copied from the tray"),
        Err(err) => warn!(%err, "could not copy the panel link"),
    }
}

fn toggle_autostart() {
    let current = crate::autostart::is_enabled().unwrap_or(false);
    match crate::autostart::set_autostart_from_tray(!current) {
        Ok(()) => info!(enabled = !current, "login start toggled from the tray"),
        Err(err) => warn!(%err, "could not change login start from the tray"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tooltip_is_truncated_rather_than_overflowing() {
        let mut data = NOTIFYICONDATAW::default();
        let long = "x".repeat(500);
        set_tip(&mut data, &long);
        // The field must stay null terminated whatever was handed in, or the
        // shell reads past the end of it.
        assert_eq!(*data.szTip.last().unwrap(), 0);
        assert_eq!(data.szTip[data.szTip.len() - 2], b'x' as u16);
    }

    #[test]
    fn a_short_tooltip_is_written_whole_and_terminated() {
        let mut data = NOTIFYICONDATAW::default();
        set_tip(&mut data, "Waypad");
        let text: String = char::decode_utf16(data.szTip.iter().copied().take_while(|c| *c != 0))
            .map(|c| c.unwrap_or('?'))
            .collect();
        assert_eq!(text, "Waypad");
    }

    #[test]
    fn the_menu_ids_are_distinct() {
        // A duplicate would silently make one entry do another's job.
        let ids = [ID_OPEN, ID_COPY, ID_AUTOSTART, ID_QUIT];
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        // Zero is what AppendMenuW uses for separators, so no command may use it.
        assert!(!ids.contains(&0));
    }
}
