//! Pointer and keyboard injection through `SendInput`.
//!
//! Windows has no equivalent of the Wayland approval dance: a process running
//! in the user's own session may synthesise input, full stop. That makes this
//! backend far simpler than the portal one, and it is why
//! [`InputBackend::prepare`] here reports readiness instead of opening a dialog.
//!
//! Two things are worth knowing before changing anything in here.
//!
//! Keysyms are the wire format on every platform, so this file translates X11
//! keysyms into virtual-key codes. Printable characters go through
//! `VkKeyScanW`, which resolves them against the *user's current layout* rather
//! than assuming US QWERTY — typing an apostrophe on an Italian keyboard has to
//! land on the key the user actually has.
//!
//! Text is not sent as keysyms at all. `KEYEVENTF_UNICODE` injects a code unit
//! directly, with no layout involved, which is both faster and correct for
//! characters the layout cannot reach. This is the one place where Windows is
//! genuinely easier than Wayland, where the same problem needs a clipboard
//! round trip.

use anyhow::{Context, bail};
use async_trait::async_trait;
use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard},
};
use tracing::{debug, warn};
use waypad_core::{
    backend::InputBackend,
    input::{ScrollAccumulator, WHEEL_UNITS_PER_DETENT},
    protocol::{ButtonState, PointerButton},
};
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAP_VIRTUAL_KEY_TYPE, MOUSE_EVENT_FLAGS,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
        MapVirtualKeyW, SendInput, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_DELETE, VK_DOWN, VK_END,
        VK_ESCAPE, VK_F1, VK_F11, VK_F12, VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU,
        VK_LSHIFT, VK_LWIN, VK_NEXT, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU,
        VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB, VK_UP, VkKeyScanW,
    },
    WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    },
};

/// The absolute coordinate space `MOUSEEVENTF_ABSOLUTE` expects.
const ABSOLUTE_RANGE_MAX: f64 = 65535.0;

/// Maximum characters accepted in one `text` command, matching the protocol cap.
const MAX_TEXT_BYTES: usize = 4096;

pub struct SendInputBackend {
    /// What this backend believes is currently held down, so a client that
    /// disconnects mid-drag does not leave the desktop wedged.
    held: Mutex<Held>,
}

#[derive(Debug, Default)]
struct Held {
    keys: HashSet<u16>,
    buttons: HashSet<u8>,
    scroll: ScrollAccumulator,
}

impl Default for SendInputBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SendInputBackend {
    pub fn new() -> Self {
        Self {
            held: Mutex::new(Held::default()),
        }
    }

    /// Takes the tracked-state lock, recovering from a poisoned mutex.
    ///
    /// A panic in one injection must not permanently disable input for the
    /// session: the tracked set may be stale afterwards, which costs at worst a
    /// missed release, while refusing the lock forever costs the whole feature.
    fn lock(&self) -> MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl InputBackend for SendInputBackend {
    fn name(&self) -> &'static str {
        "windows-sendinput"
    }

    async fn prepare(&mut self) -> anyhow::Result<serde_json::Value> {
        let (width, height) = virtual_desktop_size()?;
        Ok(serde_json::json!({
            "backend": "windows-sendinput",
            "ready": true,
            "requires_user_approval": false,
            "virtual_desktop_width": width,
            "virtual_desktop_height": height,
            "detail": "Windows injects input directly; no approval step is needed."
        }))
    }

    async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        // Rounding toward zero rather than nearest: a sub-pixel delta must not
        // become a whole pixel of drift on every frame of a slow drag.
        let dx = dx.trunc() as i32;
        let dy = dy.trunc() as i32;
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        send(&[mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    async fn pointer_move_absolute(&self, x: f64, y: f64) -> anyhow::Result<()> {
        let (nx, ny) = to_virtual_desktop_absolute(x, y)?;
        send(&[mouse_input(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> anyhow::Result<()> {
        let flags = match (&button, &state) {
            (PointerButton::Left, ButtonState::Pressed) => MOUSEEVENTF_LEFTDOWN,
            (PointerButton::Left, ButtonState::Released) => MOUSEEVENTF_LEFTUP,
            (PointerButton::Right, ButtonState::Pressed) => MOUSEEVENTF_RIGHTDOWN,
            (PointerButton::Right, ButtonState::Released) => MOUSEEVENTF_RIGHTUP,
            (PointerButton::Middle, ButtonState::Pressed) => MOUSEEVENTF_MIDDLEDOWN,
            (PointerButton::Middle, ButtonState::Released) => MOUSEEVENTF_MIDDLEUP,
        };
        send(&[mouse_input(0, 0, 0, flags)])?;
        let tag = button_tag(&button);
        let mut held = self.lock();
        match state {
            ButtonState::Pressed => {
                held.buttons.insert(tag);
            }
            ButtonState::Released => {
                held.buttons.remove(&tag);
            }
        }
        Ok(())
    }

    async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        let (horizontal, vertical) = self.lock().scroll.push(dx, dy, finish);
        let mut events = Vec::new();
        if vertical != 0 {
            events.push(mouse_input(
                0,
                0,
                vertical * WHEEL_UNITS_PER_DETENT,
                MOUSEEVENTF_WHEEL,
            ));
        }
        if horizontal != 0 {
            events.push(mouse_input(
                0,
                0,
                horizontal * WHEEL_UNITS_PER_DETENT,
                MOUSEEVENTF_HWHEEL,
            ));
        }
        if events.is_empty() {
            return Ok(());
        }
        send(&events)
    }

    async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        let mapping = keysym_to_vk(keysym)
            .with_context(|| format!("No Windows key for keysym 0x{keysym:04x}"))?;
        let pressed = matches!(state, ButtonState::Pressed);
        let mut events = Vec::new();
        // The shift needed to reach a character is part of pressing it, and is
        // released in the mirror order so a held modifier from the client is
        // never clobbered.
        if mapping.shift && pressed {
            events.push(key_input(VK_SHIFT.0, false));
        }
        events.push(key_input(mapping.vk, !pressed));
        if mapping.shift && !pressed {
            events.push(key_input(VK_SHIFT.0, true));
        }
        send(&events)?;

        let mut held = self.lock();
        if pressed {
            held.keys.insert(mapping.vk);
        } else {
            held.keys.remove(&mapping.vk);
        }
        Ok(())
    }

    async fn text(&self, text: &str) -> anyhow::Result<()> {
        if text.len() > MAX_TEXT_BYTES {
            bail!("Text input rejected: maximum length is {MAX_TEXT_BYTES} bytes");
        }
        // One event pair per UTF-16 code unit. Surrogate pairs need both halves
        // delivered in the same call, so the whole string goes in one SendInput
        // batch: splitting it could interleave another process's input between
        // the halves and produce a replacement character.
        let mut events = Vec::new();
        for unit in text.encode_utf16() {
            events.push(unicode_input(unit, false));
            events.push(unicode_input(unit, true));
        }
        if events.is_empty() {
            return Ok(());
        }
        send(&events)
    }

    fn release_all(&self) {
        let (keys, buttons) = {
            let mut held = self.lock();
            held.scroll = ScrollAccumulator::new();
            (
                held.keys.drain().collect::<Vec<_>>(),
                held.buttons.drain().collect::<Vec<_>>(),
            )
        };
        if keys.is_empty() && buttons.is_empty() {
            return;
        }
        debug!(
            keys = keys.len(),
            buttons = buttons.len(),
            "releasing input held by a departing client"
        );
        let mut events = Vec::new();
        for vk in keys {
            events.push(key_input(vk, true));
        }
        for tag in buttons {
            let flags = match tag {
                TAG_LEFT => MOUSEEVENTF_LEFTUP,
                TAG_RIGHT => MOUSEEVENTF_RIGHTUP,
                _ => MOUSEEVENTF_MIDDLEUP,
            };
            events.push(mouse_input(0, 0, 0, flags));
        }
        // Best effort by contract: the client is already gone, so there is
        // nobody to report a failure to and nothing useful to do about it.
        if let Err(err) = send(&events) {
            warn!(%err, "failed to release input held by a departing client");
        }
    }
}

/// Presses and releases one virtual key as a single atomic sequence.
///
/// Used for the multimedia keys, which are stateless taps rather than something
/// a client holds down, so they need none of the held-key tracking above.
pub fn tap_virtual_key(key: VIRTUAL_KEY) -> anyhow::Result<()> {
    send(&[key_input(key.0, false), key_input(key.0, true)])
}

const TAG_LEFT: u8 = 0;
const TAG_RIGHT: u8 = 1;
const TAG_MIDDLE: u8 = 2;

fn button_tag(button: &PointerButton) -> u8 {
    match button {
        PointerButton::Left => TAG_LEFT,
        PointerButton::Right => TAG_RIGHT,
        PointerButton::Middle => TAG_MIDDLE,
    }
}

/// A virtual key plus whether shift must be held to produce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VkMapping {
    pub vk: u16,
    pub shift: bool,
}

const fn plain(vk: VIRTUAL_KEY) -> VkMapping {
    VkMapping {
        vk: vk.0,
        shift: false,
    }
}

/// Maps an X11 keysym onto a Windows virtual key.
///
/// Named keys come from the table; anything else is treated as a character and
/// resolved against the user's active keyboard layout, so punctuation lands on
/// the key they actually have rather than the US one.
pub fn keysym_to_vk(keysym: u32) -> Option<VkMapping> {
    let named = match keysym {
        0xff08 => plain(VK_BACK),
        0xff09 => plain(VK_TAB),
        0xff0d => plain(VK_RETURN),
        0xff1b => plain(VK_ESCAPE),
        0xff50 => plain(VK_HOME),
        0xff51 => plain(VK_LEFT),
        0xff52 => plain(VK_UP),
        0xff53 => plain(VK_RIGHT),
        0xff54 => plain(VK_DOWN),
        0xff55 => plain(VK_PRIOR),
        0xff56 => plain(VK_NEXT),
        0xff57 => plain(VK_END),
        0xff63 => plain(VK_INSERT),
        0xffff => plain(VK_DELETE),
        0xffe1 => plain(VK_LSHIFT),
        0xffe2 => plain(VK_RSHIFT),
        0xffe3 => plain(VK_LCONTROL),
        0xffe4 => plain(VK_RCONTROL),
        0xffe5 => plain(VK_CAPITAL),
        0xffe9 => plain(VK_LMENU),
        0xffea => plain(VK_RMENU),
        0xffeb => plain(VK_LWIN),
        0xffec => plain(VK_RWIN),
        // F1..F10 are contiguous in both keysym and VK space; F11/F12 are not
        // contiguous in keysym space, so they are listed separately.
        0xffbe..=0xffc7 => plain(VIRTUAL_KEY(VK_F1.0 + (keysym - 0xffbe) as u16)),
        0xffc8 => plain(VK_F11),
        0xffc9 => plain(VK_F12),
        _ => return char::from_u32(keysym).and_then(char_to_vk),
    };
    Some(named)
}

/// Resolves a printable character against the current keyboard layout.
///
/// Returns `None` for characters the layout cannot type; the caller reports
/// that rather than pressing something arbitrary. Text input does not come
/// through here at all — it uses `KEYEVENTF_UNICODE` and needs no layout.
pub fn char_to_vk(value: char) -> Option<VkMapping> {
    let mut buffer = [0u16; 2];
    let encoded = value.encode_utf16(&mut buffer);
    if encoded.len() != 1 {
        // Outside the BMP: unreachable by a single key on any layout.
        return None;
    }
    // SAFETY: VkKeyScanW takes a plain UTF-16 code unit and only reads it.
    let result = unsafe { VkKeyScanW(encoded[0]) };
    if result == -1 {
        return None;
    }
    let vk = (result & 0xff) as u16;
    let modifiers = (result >> 8) & 0xff;
    // Bit 0 is shift. Ctrl and Alt combinations (bits 1 and 2) are AltGr
    // characters; injecting them as a bare key would type the wrong thing, so
    // they are declined and the caller reports an honest failure.
    if modifiers & 0b110 != 0 {
        return None;
    }
    Some(VkMapping {
        vk,
        shift: modifiers & 1 != 0,
    })
}

/// Bounds of the virtual desktop: the union of every monitor, which is the
/// space `MOUSEEVENTF_VIRTUALDESK` normalises against.
fn virtual_desktop() -> anyhow::Result<(i32, i32, i32, i32)> {
    // SAFETY: GetSystemMetrics reads a global and cannot fail; a zero result
    // means the metric is unavailable, which is checked below.
    let (x, y, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0 || height <= 0 {
        bail!("Windows reported an empty virtual desktop ({width}x{height})");
    }
    Ok((x, y, width, height))
}

fn virtual_desktop_size() -> anyhow::Result<(i32, i32)> {
    let (_, _, width, height) = virtual_desktop()?;
    Ok((width, height))
}

/// Converts a desktop pixel coordinate into the 0..65535 space `SendInput`
/// wants, clamped so a coordinate off the edge of the desktop parks the pointer
/// at the edge instead of wrapping.
///
/// Accurate to about a pixel, and no better, which is a property of the
/// platform rather than of this arithmetic: Windows rounds on the way back, and
/// on a desktop with mixed DPI scaling the scaled coordinate space the metrics
/// report does not map one-to-one onto physical pixels at all. Measured on a
/// 1920@100% + 1920@125% pair, every candidate formula lands within one pixel
/// and none lands exactly.
///
/// Two consequences worth knowing before chasing a "wrong position" report.
/// A target on the seam between two monitors can land on either side, and if
/// the neighbour is shorter in scaled pixels the pointer is then clamped up to
/// its bottom edge. And the bounding box is not the desktop: with monitors of
/// different scaled heights, its corners are dead space no monitor covers, and
/// Windows silently parks the cursor at the nearest real pixel instead.
fn to_virtual_desktop_absolute(x: f64, y: f64) -> anyhow::Result<(i32, i32)> {
    let (origin_x, origin_y, width, height) = virtual_desktop()?;
    let normalise = |value: f64, origin: i32, extent: i32| {
        // The -1 matters: without it the rightmost pixel column is unreachable,
        // which shows up as a pointer that can never quite touch the edge.
        let span = f64::from(extent - 1).max(1.0);
        let scaled = (value - f64::from(origin)) / span * ABSOLUTE_RANGE_MAX;
        scaled.round().clamp(0.0, ABSOLUTE_RANGE_MAX) as i32
    };
    Ok((
        normalise(x, origin_x, width),
        normalise(y, origin_y, height),
    ))
}

fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(vk: u16, release: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if release {
        flags |= KEYEVENTF_KEYUP;
    }
    // SAFETY: MapVirtualKeyW is a pure lookup against the active layout.
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAP_VIRTUAL_KEY_TYPE(0)) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, release: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if release {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // wVk must be zero for a Unicode event; the code unit rides in
                // wScan and no layout is consulted.
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Submits a batch of events as one atomic sequence.
///
/// `SendInput` guarantees no other process's input is interleaved within a
/// single call, which is why press/release pairs and shift-wrapped keys are
/// always sent together rather than one call at a time.
fn send(events: &[INPUT]) -> anyhow::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    // SAFETY: the slice is valid for the duration of the call and the size
    // argument is the documented `size_of::<INPUT>()`.
    let sent = unsafe { SendInput(events, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        // The usual cause is UIPI: an elevated window has focus and a
        // medium-integrity process may not inject into it. Saying so is more
        // useful than reporting a bare error code.
        bail!(
            "Windows accepted {sent} of {} input events. A window running as administrator most likely has focus; Waypad cannot inject into it unless it also runs elevated.",
            events.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_named_keysyms_the_client_sends() {
        assert_eq!(keysym_to_vk(0xff0d).unwrap().vk, VK_RETURN.0);
        assert_eq!(keysym_to_vk(0xff1b).unwrap().vk, VK_ESCAPE.0);
        assert_eq!(keysym_to_vk(0xff51).unwrap().vk, VK_LEFT.0);
        assert_eq!(keysym_to_vk(0xffe3).unwrap().vk, VK_LCONTROL.0);
        assert_eq!(keysym_to_vk(0xffeb).unwrap().vk, VK_LWIN.0);
        assert_eq!(keysym_to_vk(0xffff).unwrap().vk, VK_DELETE.0);
    }

    #[test]
    fn maps_the_whole_function_row_including_the_discontinuity() {
        assert_eq!(keysym_to_vk(0xffbe).unwrap().vk, VK_F1.0);
        assert_eq!(keysym_to_vk(0xffc7).unwrap().vk, VK_F1.0 + 9);
        // F11 and F12 are not contiguous with F10 in keysym space, so a naive
        // range would map them onto F13 and F14.
        assert_eq!(keysym_to_vk(0xffc8).unwrap().vk, VK_F11.0);
        assert_eq!(keysym_to_vk(0xffc9).unwrap().vk, VK_F12.0);
    }

    #[test]
    fn resolves_letters_against_the_active_layout() {
        // Every layout can type an unshifted lowercase letter and its shifted
        // uppercase counterpart on the same key.
        let lower = keysym_to_vk('a' as u32).expect("layout can type 'a'");
        let upper = keysym_to_vk('A' as u32).expect("layout can type 'A'");
        assert_eq!(lower.vk, upper.vk);
        assert!(!lower.shift);
        assert!(upper.shift);
    }

    #[test]
    fn declines_characters_no_single_key_can_produce() {
        // Outside the BMP: no layout has a key for it, and text() is the right
        // path for such characters anyway.
        assert_eq!(char_to_vk('\u{1F600}'), None);
    }

    #[test]
    fn absolute_coordinates_span_the_full_range_and_clamp_outside_it() {
        let (_, _, width, height) = virtual_desktop().expect("a desktop exists on a test host");
        let origin = to_virtual_desktop_absolute(0.0, 0.0).unwrap();
        let corner =
            to_virtual_desktop_absolute(f64::from(width - 1), f64::from(height - 1)).unwrap();
        assert_eq!(corner, (65535, 65535));
        assert!(origin.0 <= 1 && origin.1 <= 1, "{origin:?}");

        // Off the edge parks at the edge rather than wrapping to the far side.
        let far = to_virtual_desktop_absolute(999_999.0, 999_999.0).unwrap();
        assert_eq!(far, (65535, 65535));
        let negative = to_virtual_desktop_absolute(-999_999.0, -999_999.0).unwrap();
        assert_eq!(negative, (0, 0));
    }

    #[tokio::test]
    async fn a_sub_pixel_drag_never_drifts() {
        // Truncation, not rounding: 0.4 px per event repeated forever must move
        // the pointer zero pixels, not one per event.
        let backend = SendInputBackend::new();
        for _ in 0..50 {
            backend.pointer_move(0.4, 0.4).await.unwrap();
        }
    }

    #[tokio::test]
    async fn release_all_is_a_no_op_when_nothing_is_held() {
        let backend = SendInputBackend::new();
        backend.release_all();
        assert!(backend.lock().keys.is_empty());
        assert!(backend.lock().buttons.is_empty());
    }
}

/// Verifies the absolute-coordinate maths against the real cursor.
///
/// Ignored by default because it moves the mouse of whoever runs it. Run it
/// explicitly with `cargo test -p waypad-windows -- --ignored` on a machine
/// with a desktop: the round-trip through `SendInput` is the only way to catch
/// an off-by-one in the 0..65535 normalisation, which otherwise shows up as a
/// pointer that cannot quite reach the right or bottom edge.
#[cfg(test)]
mod live_tests {
    use super::*;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    fn cursor() -> POINT {
        let mut point = POINT::default();
        // SAFETY: GetCursorPos writes one POINT and reports failure via Result.
        unsafe { GetCursorPos(&mut point) }.expect("cursor position is readable");
        point
    }

    /// True when this session can actually move a pointer.
    ///
    /// A locked, disconnected, or headless session still reports monitors and
    /// still accepts `SendInput` without error — it simply discards the input
    /// and pins the cursor at the origin. Detecting that up front turns a
    /// baffling "landed at (0,0)" failure into a sentence that says what to do.
    fn has_interactive_pointer() -> bool {
        use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
        let start = cursor();
        // SAFETY: SetCursorPos takes two integers and reports failure via Result.
        let moved = unsafe { SetCursorPos(start.x, start.y) }.is_ok();
        moved && !(start.x == 0 && start.y == 0 && !moved)
    }

    #[tokio::test]
    #[ignore = "moves the mouse of whoever runs it"]
    async fn the_pointer_lands_where_it_was_told_to() {
        use windows::Win32::UI::WindowsAndMessaging::{SM_CXSCREEN, SM_CYSCREEN};

        assert!(
            has_interactive_pointer(),
            concat!(
                "this Windows session has no interactive pointer: SetCursorPos is refused ",
                "and the cursor is pinned at the origin. Unlock the desktop and run this on ",
                "the console session. Injection is accepted and then silently discarded ",
                "otherwise, which is exactly what a user sees if they run the daemon from a ",
                "locked session."
            )
        );

        let backend = SendInputBackend::new();
        let (origin_x, origin_y, width, _height) = virtual_desktop().unwrap();
        // SAFETY: GetSystemMetrics reads a global and cannot fail.
        let (primary_width, primary_height) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let restore = cursor();

        // Every target has to be a point some monitor actually covers. The
        // bottom-right of the virtual desktop's bounding box is not: with two
        // monitors at different DPI scaling the shorter one leaves dead space
        // below it, and Windows quietly parks the cursor at the nearest real
        // pixel instead. Asserting against the bounding box therefore fails on
        // exactly the setups this mapping most needs to be right for.
        for (label, target_x, target_y) in [
            ("origin", origin_x, origin_y),
            (
                "primary centre",
                origin_x + primary_width / 2,
                origin_y + primary_height / 2,
            ),
            (
                "primary lower right, off the seam",
                origin_x + primary_width - 40,
                origin_y + primary_height - 40,
            ),
            // Inset from the far edge for the same reason: the last column
            // belongs to whichever monitor rounding picks.
            ("far right, top area", origin_x + width - 40, origin_y + 40),
        ] {
            // Retried because a human using the machine moves the mouse between
            // the injection and the readback, which is interference rather than
            // a failure. Three chances is enough to get one clean landing
            // without the test hanging around if the mapping is genuinely wrong.
            let mut landed = POINT::default();
            let mut ok = false;
            for _ in 0..3 {
                backend
                    .pointer_move_absolute(f64::from(target_x), f64::from(target_y))
                    .await
                    .unwrap();
                // SendInput is asynchronous with respect to the cursor: the move
                // is queued, so the position is read after the queue drains.
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                landed = cursor();
                // One pixel of slack: the 0..65535 grid is coarser than a large
                // desktop, so an exact landing is not always representable.
                if (landed.x - target_x).abs() <= 1 && (landed.y - target_y).abs() <= 1 {
                    ok = true;
                    break;
                }
            }
            assert!(
                ok,
                "{label}: asked for ({target_x},{target_y}), landed at ({},{}) three times",
                landed.x, landed.y
            );
        }

        backend
            .pointer_move_absolute(f64::from(restore.x), f64::from(restore.y))
            .await
            .unwrap();
    }
}
