//! Linux `uinput` plumbing and the Waypad virtual pointer/keyboard devices.
//!
//! Wayland has no global input-injection API by design, and the `RemoteDesktop`
//! portal is unavailable on several wlroots stacks (xdg-desktop-portal-hyprland
//! does not implement it). The Hyprland IPC fallback can warp the cursor, but
//! `sendkeystate` delivers a synthetic event to a *window* rather than to the
//! seat, so no implicit pointer grab is created and dragging cannot work.
//!
//! Creating real evdev devices through `uinput` sidesteps both problems: the
//! compositor sees ordinary input hardware, so implicit grabs, drag-and-drop and
//! relative motion for games behave exactly as they do with a physical mouse.
//! Two pointer devices are created because libinput applies pointer
//! acceleration to relative motion, which would make absolute positioning for
//! remote screen viewing drift:
//!
//! - a relative pointer for pad mode and games,
//! - an absolute pointer (`INPUT_PROP_POINTER`) for pixel-exact screen mirroring.

use anyhow::{Context, bail};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Write,
    mem,
    os::fd::AsRawFd,
    path::Path,
    sync::Mutex,
};
use tracing::{debug, info, warn};
use waypad_core::protocol::{ButtonState, PointerButton};

pub(crate) const UINPUT_PATH: &str = "/dev/uinput";
pub(crate) const UINPUT_IOCTL_BASE: u8 = b'U';
pub(crate) const UINPUT_MAX_NAME_SIZE: usize = 80;
pub(crate) const ABS_CNT: usize = 0x40;

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_REL: u16 = 0x02;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const SYN_REPORT: u16 = 0x00;

pub(crate) const BUS_USB: u16 = 0x03;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const INPUT_PROP_POINTER: u16 = 0x00;

/// Absolute axes are reported in a device-independent range and mapped onto the
/// desktop bounding box, so monitor hotplug only changes the mapping constants.
const ABS_RANGE_MAX: i32 = 65_535;

/// libinput reports one wheel detent as 120 high-resolution units.
const WHEEL_HI_RES_PER_DETENT: f64 = 120.0;

/// Pixels of accumulated scroll that make up a single wheel detent.
const SCROLL_PIXELS_PER_DETENT: f64 = 24.0;

const VENDOR_ID: u16 = 0x1209;
const PRODUCT_RELATIVE: u16 = 0x5752;
const PRODUCT_ABSOLUTE: u16 = 0x5753;
const PRODUCT_KEYBOARD: u16 = 0x5754;

/// Reports whether the daemon can create virtual pointer/keyboard devices.
pub fn detect_virtual_pointer_support() -> (bool, String) {
    if !Path::new(UINPUT_PATH).exists() {
        return (
            false,
            format!(
                "Virtual pointer input requires {UINPUT_PATH}. Load the uinput module and allow the Waypad user to open the device."
            ),
        );
    }
    match OpenOptions::new().write(true).open(UINPUT_PATH) {
        Ok(_) => (
            true,
            "Pointer, buttons, scroll and keyboard are injected through real Linux uinput devices, so drag, text selection and relative game input behave like physical hardware."
                .into(),
        ),
        Err(err) => (
            false,
            format!(
                "Virtual pointer input requires write access to {UINPUT_PATH}; current user cannot open it: {err}"
            ),
        ),
    }
}

/// The bounding box that absolute pointer coordinates are mapped onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Default for DesktopBounds {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
    }
}

impl DesktopBounds {
    /// Computes the union of all connected monitors from the compositor.
    pub fn detect() -> Self {
        match hyprland_desktop_bounds() {
            Some(bounds) => bounds,
            None => {
                debug!("could not read monitor layout; using default desktop bounds");
                Self::default()
            }
        }
    }

    /// Maps a desktop pixel coordinate onto the device-independent absolute range.
    fn to_abs(self, x: f64, y: f64) -> (i32, i32) {
        let width = f64::from(self.width.max(1));
        let height = f64::from(self.height.max(1));
        let normalized_x = (x - f64::from(self.x)) / width;
        let normalized_y = (y - f64::from(self.y)) / height;
        (
            scale_to_abs_range(normalized_x),
            scale_to_abs_range(normalized_y),
        )
    }
}

fn scale_to_abs_range(normalized: f64) -> i32 {
    let clamped = normalized.clamp(0.0, 1.0);
    (clamped * f64::from(ABS_RANGE_MAX)).round() as i32
}

fn hyprland_desktop_bounds() -> Option<DesktopBounds> {
    let output = std::process::Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let monitors: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let monitors = monitors.as_array()?;
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for monitor in monitors {
        let x = monitor.get("x")?.as_i64()?;
        let y = monitor.get("y")?.as_i64()?;
        let width = monitor.get("width")?.as_i64()?;
        let height = monitor.get("height")?.as_i64()?;
        let scale = monitor.get("scale").and_then(|s| s.as_f64()).unwrap_or(1.0);
        // Hyprland reports physical pixels; the cursor lives in logical pixels.
        let logical_width = (width as f64 / scale.max(0.1)).round() as i64;
        let logical_height = (height as f64 / scale.max(0.1)).round() as i64;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + logical_width);
        max_y = max_y.max(y + logical_height);
    }
    if min_x == i64::MAX || max_x <= min_x || max_y <= min_y {
        return None;
    }
    Some(DesktopBounds {
        x: min_x as i32,
        y: min_y as i32,
        width: (max_x - min_x) as u32,
        height: (max_y - min_y) as u32,
    })
}

/// Virtual pointer and keyboard devices backed by `uinput`.
#[derive(Debug)]
pub struct VirtualInputBackend {
    devices: Mutex<VirtualDevices>,
}

#[derive(Debug)]
struct VirtualDevices {
    relative: File,
    absolute: File,
    keyboard: File,
    bounds: DesktopBounds,
    scroll_x_remainder: f64,
    scroll_y_remainder: f64,
    pressed_keys: HashSet<u16>,
    pressed_buttons: HashSet<u16>,
}

impl VirtualInputBackend {
    pub fn create() -> anyhow::Result<Self> {
        let relative = create_relative_pointer()?;
        let absolute = create_absolute_pointer()?;
        let keyboard = create_keyboard()?;
        let bounds = DesktopBounds::detect();
        info!(
            desktop_width = bounds.width,
            desktop_height = bounds.height,
            desktop_x = bounds.x,
            desktop_y = bounds.y,
            "created Waypad uinput virtual pointer and keyboard"
        );
        Ok(Self {
            devices: Mutex::new(VirtualDevices {
                relative,
                absolute,
                keyboard,
                bounds,
                scroll_x_remainder: 0.0,
                scroll_y_remainder: 0.0,
                pressed_keys: HashSet::new(),
                pressed_buttons: HashSet::new(),
            }),
        })
    }

    pub fn prepare(&self) -> anyhow::Result<serde_json::Value> {
        let bounds = self.lock()?.bounds;
        Ok(serde_json::json!({
            "backend": "uinput",
            "status": "ready",
            "desktop_bounds": {
                "x": bounds.x,
                "y": bounds.y,
                "width": bounds.width,
                "height": bounds.height,
            },
            "limitations": "text injection assumes a US keymap for punctuation; unmapped characters fall back to clipboard paste"
        }))
    }

    /// Re-reads the monitor layout so absolute pointing stays correct after hotplug.
    pub fn refresh_desktop_bounds(&self) -> anyhow::Result<DesktopBounds> {
        let bounds = DesktopBounds::detect();
        let mut devices = self.lock()?;
        if devices.bounds != bounds {
            info!(
                ?bounds,
                "desktop bounds changed; remapping absolute pointer"
            );
            devices.bounds = bounds;
        }
        Ok(bounds)
    }

    pub fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        if !dx.is_finite() || !dy.is_finite() {
            bail!("Pointer delta rejected as invalid");
        }
        let dx = dx.round() as i32;
        let dy = dy.round() as i32;
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let mut devices = self.lock()?;
        if dx != 0 {
            devices.emit_relative(EV_REL, REL_X, dx)?;
        }
        if dy != 0 {
            devices.emit_relative(EV_REL, REL_Y, dy)?;
        }
        devices.sync_relative()
    }

    pub fn pointer_move_absolute(&self, x: f64, y: f64) -> anyhow::Result<()> {
        if !x.is_finite() || !y.is_finite() {
            bail!("Absolute pointer coordinate rejected as invalid");
        }
        let mut devices = self.lock()?;
        let (abs_x, abs_y) = devices.bounds.to_abs(x, y);
        devices.emit_absolute(EV_ABS, ABS_X, abs_x)?;
        devices.emit_absolute(EV_ABS, ABS_Y, abs_y)?;
        devices.sync_absolute()
    }

    pub fn pointer_button(&self, button: PointerButton, state: ButtonState) -> anyhow::Result<()> {
        let code = uinput_code(&button);
        let value = uinput_value(&state);
        let mut devices = self.lock()?;
        let already_pressed = devices.pressed_buttons.contains(&code);
        if (value == 1) == already_pressed {
            return Ok(());
        }
        if value == 1 {
            devices.pressed_buttons.insert(code);
        } else {
            devices.pressed_buttons.remove(&code);
        }
        debug!(?button, code, value, "virtual pointer button");
        // Buttons go through the relative device so the press, the implicit grab
        // and any subsequent relative motion all originate from one seat device.
        devices.emit_relative(EV_KEY, code, value)?;
        devices.sync_relative()
    }

    pub fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        if !dx.is_finite() || !dy.is_finite() {
            bail!("Scroll delta rejected as invalid");
        }
        let mut devices = self.lock()?;
        devices.scroll_x_remainder += dx;
        devices.scroll_y_remainder += dy;
        let horizontal = take_scroll_detents(&mut devices.scroll_x_remainder, finish);
        let vertical = take_scroll_detents(&mut devices.scroll_y_remainder, finish);
        if horizontal == 0 && vertical == 0 {
            return Ok(());
        }
        if vertical != 0 {
            devices.emit_relative(EV_REL, REL_WHEEL, vertical)?;
            devices.emit_relative(
                EV_REL,
                REL_WHEEL_HI_RES,
                (f64::from(vertical) * WHEEL_HI_RES_PER_DETENT) as i32,
            )?;
        }
        if horizontal != 0 {
            devices.emit_relative(EV_REL, REL_HWHEEL, horizontal)?;
            devices.emit_relative(
                EV_REL,
                REL_HWHEEL_HI_RES,
                (f64::from(horizontal) * WHEEL_HI_RES_PER_DETENT) as i32,
            )?;
        }
        devices.sync_relative()
    }

    pub fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        let mapping = keysym_to_key(keysym)
            .with_context(|| format!("Unsupported keysym for uinput: 0x{keysym:x}"))?;
        let value = uinput_value(&state);
        let mut devices = self.lock()?;
        if value == 1 {
            devices.pressed_keys.insert(mapping.code);
        } else {
            devices.pressed_keys.remove(&mapping.code);
        }
        if mapping.shift && value == 1 {
            devices.emit_keyboard(EV_KEY, KEY_LEFTSHIFT, 1)?;
        }
        devices.emit_keyboard(EV_KEY, mapping.code, value)?;
        if mapping.shift && value == 0 {
            devices.emit_keyboard(EV_KEY, KEY_LEFTSHIFT, 0)?;
        }
        devices.sync_keyboard()
    }

    /// Types printable text as key events. Returns `false` when a character has
    /// no US-keymap equivalent, so the caller can fall back to clipboard paste.
    pub fn text(&self, text: &str) -> anyhow::Result<bool> {
        let mappings: Option<Vec<KeyMapping>> = text.chars().map(char_to_key).collect();
        let Some(mappings) = mappings else {
            return Ok(false);
        };
        let mut devices = self.lock()?;
        for mapping in mappings {
            if mapping.shift {
                devices.emit_keyboard(EV_KEY, KEY_LEFTSHIFT, 1)?;
            }
            devices.emit_keyboard(EV_KEY, mapping.code, 1)?;
            devices.emit_keyboard(EV_KEY, mapping.code, 0)?;
            if mapping.shift {
                devices.emit_keyboard(EV_KEY, KEY_LEFTSHIFT, 0)?;
            }
            devices.sync_keyboard()?;
        }
        Ok(true)
    }

    /// Releases every key and button still held, so a dropped connection cannot
    /// leave the desktop with a stuck modifier or a half-finished drag.
    pub fn release_all(&self) -> anyhow::Result<()> {
        let mut devices = self.lock()?;
        let buttons: Vec<u16> = devices.pressed_buttons.iter().copied().collect();
        for code in buttons {
            devices.emit_relative(EV_KEY, code, 0)?;
        }
        devices.pressed_buttons.clear();
        if !devices.pressed_buttons.is_empty() {
            devices.sync_relative()?;
        }
        let keys: Vec<u16> = devices.pressed_keys.iter().copied().collect();
        for code in keys {
            devices.emit_keyboard(EV_KEY, code, 0)?;
        }
        devices.pressed_keys.clear();
        devices.emit_keyboard(EV_KEY, KEY_LEFTSHIFT, 0)?;
        devices.sync_relative()?;
        devices.sync_keyboard()
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, VirtualDevices>> {
        self.devices
            .lock()
            .map_err(|_| anyhow::anyhow!("virtual input device state poisoned"))
    }
}

impl Drop for VirtualInputBackend {
    fn drop(&mut self) {
        if let Err(err) = self.release_all() {
            warn!(%err, "failed to release virtual input state before drop");
        }
        let Ok(devices) = self.devices.lock() else {
            return;
        };
        for (label, file) in [
            ("relative pointer", &devices.relative),
            ("absolute pointer", &devices.absolute),
            ("keyboard", &devices.keyboard),
        ] {
            if let Err(err) = ioctl_noarg(file, ui_dev_destroy()) {
                warn!(%err, label, "failed to destroy uinput device");
            }
        }
    }
}

impl VirtualDevices {
    fn emit_relative(&mut self, event_type: u16, code: u16, value: i32) -> anyhow::Result<()> {
        emit(&mut self.relative, event_type, code, value)
    }

    fn emit_absolute(&mut self, event_type: u16, code: u16, value: i32) -> anyhow::Result<()> {
        emit(&mut self.absolute, event_type, code, value)
    }

    fn emit_keyboard(&mut self, event_type: u16, code: u16, value: i32) -> anyhow::Result<()> {
        emit(&mut self.keyboard, event_type, code, value)
    }

    fn sync_relative(&mut self) -> anyhow::Result<()> {
        emit(&mut self.relative, EV_SYN, SYN_REPORT, 0)?;
        self.relative.flush().context("flushing relative pointer")
    }

    fn sync_absolute(&mut self) -> anyhow::Result<()> {
        emit(&mut self.absolute, EV_SYN, SYN_REPORT, 0)?;
        self.absolute.flush().context("flushing absolute pointer")
    }

    fn sync_keyboard(&mut self) -> anyhow::Result<()> {
        emit(&mut self.keyboard, EV_SYN, SYN_REPORT, 0)?;
        self.keyboard.flush().context("flushing virtual keyboard")
    }
}

fn emit(file: &mut File, event_type: u16, code: u16, value: i32) -> anyhow::Result<()> {
    let event = InputEvent {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        event_type,
        code,
        value,
    };
    write_struct(file, &event)
}

fn take_scroll_detents(remainder: &mut f64, finish: bool) -> i32 {
    if finish && remainder.abs() > SCROLL_PIXELS_PER_DETENT / 8.0 {
        let direction = if *remainder > 0.0 { 1 } else { -1 };
        *remainder = 0.0;
        return direction;
    }
    let detents = (*remainder / SCROLL_PIXELS_PER_DETENT).trunc();
    if detents == 0.0 {
        return 0;
    }
    *remainder -= detents * SCROLL_PIXELS_PER_DETENT;
    detents as i32
}

fn create_relative_pointer() -> anyhow::Result<File> {
    let mut file = open_uinput()?;
    set_evbit(&file, EV_KEY)?;
    set_evbit(&file, EV_REL)?;
    for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
        set_keybit(&file, code)?;
    }
    for code in [
        REL_X,
        REL_Y,
        REL_WHEEL,
        REL_HWHEEL,
        REL_WHEEL_HI_RES,
        REL_HWHEEL_HI_RES,
    ] {
        set_relbit(&file, code)?;
    }
    let mut device = UInputUserDev::named("Waypad Virtual Pointer");
    device.id.bustype = BUS_USB;
    device.id.vendor = VENDOR_ID;
    device.id.product = PRODUCT_RELATIVE;
    device.id.version = 1;
    write_struct(&mut file, &device).context("writing uinput relative pointer setup")?;
    ioctl_noarg(&file, ui_dev_create()).context("creating uinput relative pointer")?;
    Ok(file)
}

fn create_absolute_pointer() -> anyhow::Result<File> {
    let mut file = open_uinput()?;
    set_evbit(&file, EV_KEY)?;
    set_evbit(&file, EV_ABS)?;
    for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
        set_keybit(&file, code)?;
    }
    for code in [ABS_X, ABS_Y] {
        set_absbit(&file, code)?;
    }
    // Without INPUT_PROP_POINTER libinput would classify the absolute axes as a
    // touchscreen and bind them to a single output instead of the whole desktop.
    set_propbit(&file, INPUT_PROP_POINTER)?;
    let mut device = UInputUserDev::named("Waypad Virtual Absolute Pointer");
    device.id.bustype = BUS_USB;
    device.id.vendor = VENDOR_ID;
    device.id.product = PRODUCT_ABSOLUTE;
    device.id.version = 1;
    device.absmax[ABS_X as usize] = ABS_RANGE_MAX;
    device.absmax[ABS_Y as usize] = ABS_RANGE_MAX;
    write_struct(&mut file, &device).context("writing uinput absolute pointer setup")?;
    ioctl_noarg(&file, ui_dev_create()).context("creating uinput absolute pointer")?;
    Ok(file)
}

fn create_keyboard() -> anyhow::Result<File> {
    let mut file = open_uinput()?;
    set_evbit(&file, EV_KEY)?;
    // Cover the standard keyboard range so any mapped keycode is accepted.
    for code in 1..=KEY_MAX {
        set_keybit(&file, code)?;
    }
    let mut device = UInputUserDev::named("Waypad Virtual Keyboard");
    device.id.bustype = BUS_USB;
    device.id.vendor = VENDOR_ID;
    device.id.product = PRODUCT_KEYBOARD;
    device.id.version = 1;
    write_struct(&mut file, &device).context("writing uinput keyboard setup")?;
    ioctl_noarg(&file, ui_dev_create()).context("creating uinput keyboard")?;
    Ok(file)
}

fn open_uinput() -> anyhow::Result<File> {
    OpenOptions::new()
        .write(true)
        .open(UINPUT_PATH)
        .with_context(|| format!("opening {UINPUT_PATH}"))
}

/// Free functions rather than inherent methods: the protocol types belong to
/// the core crate, so only a trait or a function can extend them here, and a
/// one-line trait would not earn its keep.
fn uinput_code(button: &PointerButton) -> u16 {
    match button {
        PointerButton::Left => BTN_LEFT,
        PointerButton::Right => BTN_RIGHT,
        PointerButton::Middle => BTN_MIDDLE,
    }
}

fn uinput_value(state: &ButtonState) -> i32 {
    match state {
        ButtonState::Pressed => 1,
        ButtonState::Released => 0,
    }
}

// ---------------------------------------------------------------------------
// Keysym mapping
// ---------------------------------------------------------------------------

const KEY_MAX: u16 = 0xf0;

const KEY_ESC: u16 = 1;
const KEY_MINUS: u16 = 12;
const KEY_EQUAL: u16 = 13;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_GRAVE: u16 = 41;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_BACKSLASH: u16 = 43;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_SLASH: u16 = 53;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_CAPSLOCK: u16 = 58;
const KEY_F1: u16 = 59;
const KEY_F11: u16 = 87;
const KEY_F12: u16 = 88;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_HOME: u16 = 102;
const KEY_UP: u16 = 103;
const KEY_PAGEUP: u16 = 104;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_END: u16 = 107;
const KEY_DOWN: u16 = 108;
const KEY_PAGEDOWN: u16 = 109;
const KEY_INSERT: u16 = 110;
const KEY_DELETE: u16 = 111;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;

/// A keycode plus whether Shift must be held to produce the requested symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMapping {
    pub code: u16,
    pub shift: bool,
}

const fn plain(code: u16) -> KeyMapping {
    KeyMapping { code, shift: false }
}

const fn shifted(code: u16) -> KeyMapping {
    KeyMapping { code, shift: true }
}

/// Maps an X11 keysym onto a Linux keycode.
pub fn keysym_to_key(keysym: u32) -> Option<KeyMapping> {
    let named = match keysym {
        0xff08 => plain(KEY_BACKSPACE),
        0xff09 => plain(KEY_TAB),
        0xff0d => plain(KEY_ENTER),
        0xff1b => plain(KEY_ESC),
        0xff50 => plain(KEY_HOME),
        0xff51 => plain(KEY_LEFT),
        0xff52 => plain(KEY_UP),
        0xff53 => plain(KEY_RIGHT),
        0xff54 => plain(KEY_DOWN),
        0xff55 => plain(KEY_PAGEUP),
        0xff56 => plain(KEY_PAGEDOWN),
        0xff57 => plain(KEY_END),
        0xff63 => plain(KEY_INSERT),
        0xffff => plain(KEY_DELETE),
        0xffe1 => plain(KEY_LEFTSHIFT),
        0xffe2 => plain(KEY_RIGHTSHIFT),
        0xffe3 => plain(KEY_LEFTCTRL),
        0xffe4 => plain(KEY_RIGHTCTRL),
        0xffe5 => plain(KEY_CAPSLOCK),
        0xffe9 => plain(KEY_LEFTALT),
        0xffea => plain(KEY_RIGHTALT),
        0xffeb => plain(KEY_LEFTMETA),
        0xffec => plain(KEY_RIGHTMETA),
        // F1..F10 are contiguous, F11/F12 are not.
        0xffbe..=0xffc7 => plain(KEY_F1 + (keysym - 0xffbe) as u16),
        0xffc8 => plain(KEY_F11),
        0xffc9 => plain(KEY_F12),
        _ => return char::from_u32(keysym).and_then(char_to_key),
    };
    Some(named)
}

/// Maps a printable character onto a Linux keycode assuming a US keymap.
pub fn char_to_key(value: char) -> Option<KeyMapping> {
    // Letters and digits sit on the same physical keys in every QWERTY layout,
    // so only punctuation is genuinely keymap dependent.
    let mapping = match value {
        'a'..='z' => plain(letter_code(value)),
        'A'..='Z' => shifted(letter_code(value.to_ascii_lowercase())),
        '1'..='9' => plain(2 + (value as u16 - '1' as u16)),
        '0' => plain(11),
        '!' => shifted(2),
        '@' => shifted(3),
        '#' => shifted(4),
        '$' => shifted(5),
        '%' => shifted(6),
        '^' => shifted(7),
        '&' => shifted(8),
        '*' => shifted(9),
        '(' => shifted(10),
        ')' => shifted(11),
        ' ' => plain(KEY_SPACE),
        '\n' | '\r' => plain(KEY_ENTER),
        '\t' => plain(KEY_TAB),
        '-' => plain(KEY_MINUS),
        '_' => shifted(KEY_MINUS),
        '=' => plain(KEY_EQUAL),
        '+' => shifted(KEY_EQUAL),
        '[' => plain(KEY_LEFTBRACE),
        '{' => shifted(KEY_LEFTBRACE),
        ']' => plain(KEY_RIGHTBRACE),
        '}' => shifted(KEY_RIGHTBRACE),
        ';' => plain(KEY_SEMICOLON),
        ':' => shifted(KEY_SEMICOLON),
        '\'' => plain(KEY_APOSTROPHE),
        '"' => shifted(KEY_APOSTROPHE),
        '`' => plain(KEY_GRAVE),
        '~' => shifted(KEY_GRAVE),
        '\\' => plain(KEY_BACKSLASH),
        '|' => shifted(KEY_BACKSLASH),
        ',' => plain(KEY_COMMA),
        '<' => shifted(KEY_COMMA),
        '.' => plain(KEY_DOT),
        '>' => shifted(KEY_DOT),
        '/' => plain(KEY_SLASH),
        '?' => shifted(KEY_SLASH),
        _ => return None,
    };
    Some(mapping)
}

/// US-keymap letter row order: the alphabet is not contiguous in keycode space.
fn letter_code(value: char) -> u16 {
    const LETTERS: [(char, u16); 26] = [
        ('q', 16),
        ('w', 17),
        ('e', 18),
        ('r', 19),
        ('t', 20),
        ('y', 21),
        ('u', 22),
        ('i', 23),
        ('o', 24),
        ('p', 25),
        ('a', 30),
        ('s', 31),
        ('d', 32),
        ('f', 33),
        ('g', 34),
        ('h', 35),
        ('j', 36),
        ('k', 37),
        ('l', 38),
        ('z', 44),
        ('x', 45),
        ('c', 46),
        ('v', 47),
        ('b', 48),
        ('n', 49),
        ('m', 50),
    ];
    LETTERS
        .iter()
        .find_map(|(letter, code)| (*letter == value).then_some(*code))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Shared low-level uinput plumbing
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
pub(crate) struct UInputUserDev {
    pub name: [u8; UINPUT_MAX_NAME_SIZE],
    pub id: InputId,
    pub ff_effects_max: u32,
    pub absmax: [i32; ABS_CNT],
    pub absmin: [i32; ABS_CNT],
    pub absfuzz: [i32; ABS_CNT],
    pub absflat: [i32; ABS_CNT],
}

impl UInputUserDev {
    pub(crate) fn named(name: &str) -> Self {
        let mut device = Self {
            name: [0; UINPUT_MAX_NAME_SIZE],
            id: InputId {
                bustype: 0,
                vendor: 0,
                product: 0,
                version: 0,
            },
            ff_effects_max: 0,
            absmax: [0; ABS_CNT],
            absmin: [0; ABS_CNT],
            absfuzz: [0; ABS_CNT],
            absflat: [0; ABS_CNT],
        };
        let bytes = name.as_bytes();
        let len = bytes.len().min(UINPUT_MAX_NAME_SIZE - 1);
        device.name[..len].copy_from_slice(&bytes[..len]);
        device
    }
}

#[repr(C)]
pub(crate) struct InputEvent {
    pub time: libc::timeval,
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
}

pub(crate) fn write_struct<T>(file: &mut File, value: &T) -> anyhow::Result<()> {
    let bytes =
        unsafe { std::slice::from_raw_parts(value as *const T as *const u8, mem::size_of::<T>()) };
    file.write_all(bytes).map_err(Into::into)
}

pub(crate) fn set_evbit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_evbit(), code)
}

pub(crate) fn set_keybit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_keybit(), code)
}

pub(crate) fn set_relbit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_relbit(), code)
}

pub(crate) fn set_absbit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_absbit(), code)
}

pub(crate) fn set_propbit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_propbit(), code)
}

pub(crate) fn ioctl_int(file: &File, request: libc::c_ulong, value: u16) -> anyhow::Result<()> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, value as libc::c_int) };
    if result < 0 {
        Err(std::io::Error::last_os_error()).context("uinput ioctl failed")
    } else {
        Ok(())
    }
}

pub(crate) fn ioctl_noarg(file: &File, request: libc::c_ulong) -> anyhow::Result<()> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request) };
    if result < 0 {
        Err(std::io::Error::last_os_error()).context("uinput ioctl failed")
    } else {
        Ok(())
    }
}

pub(crate) fn ui_dev_create() -> libc::c_ulong {
    ioc(0, UINPUT_IOCTL_BASE, 1, 0)
}

pub(crate) fn ui_dev_destroy() -> libc::c_ulong {
    ioc(0, UINPUT_IOCTL_BASE, 2, 0)
}

fn ui_set_evbit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 100, mem::size_of::<libc::c_int>())
}

fn ui_set_keybit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 101, mem::size_of::<libc::c_int>())
}

fn ui_set_relbit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 102, mem::size_of::<libc::c_int>())
}

fn ui_set_absbit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 103, mem::size_of::<libc::c_int>())
}

fn ui_set_propbit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 110, mem::size_of::<libc::c_int>())
}

pub(crate) fn iow(io_type: u8, nr: u8, size: usize) -> libc::c_ulong {
    ioc(1, io_type, nr, size)
}

pub(crate) fn ioc(dir: u8, io_type: u8, nr: u8, size: usize) -> libc::c_ulong {
    const NRSHIFT: u32 = 0;
    const TYPESHIFT: u32 = 8;
    const SIZESHIFT: u32 = 16;
    const DIRSHIFT: u32 = 30;
    ((dir as libc::c_ulong) << DIRSHIFT)
        | ((io_type as libc::c_ulong) << TYPESHIFT)
        | ((nr as libc::c_ulong) << NRSHIFT)
        | ((size as libc::c_ulong) << SIZESHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_desktop_pixels_onto_the_absolute_range() {
        let bounds = DesktopBounds {
            x: 0,
            y: 0,
            width: 3840,
            height: 1080,
        };
        assert_eq!(bounds.to_abs(0.0, 0.0), (0, 0));
        assert_eq!(
            bounds.to_abs(3840.0, 1080.0),
            (ABS_RANGE_MAX, ABS_RANGE_MAX)
        );
        let (x, y) = bounds.to_abs(1920.0, 540.0);
        assert_eq!(x, ABS_RANGE_MAX / 2 + 1);
        assert_eq!(y, ABS_RANGE_MAX / 2 + 1);
    }

    #[test]
    fn offsets_absolute_coordinates_by_the_desktop_origin() {
        let bounds = DesktopBounds {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        assert_eq!(bounds.to_abs(-1920.0, 0.0), (0, 0));
        assert_eq!(
            bounds.to_abs(1920.0, 1080.0),
            (ABS_RANGE_MAX, ABS_RANGE_MAX)
        );
    }

    #[test]
    fn clamps_absolute_coordinates_outside_the_desktop() {
        let bounds = DesktopBounds::default();
        assert_eq!(bounds.to_abs(-500.0, -500.0), (0, 0));
        assert_eq!(
            bounds.to_abs(9999.0, 9999.0),
            (ABS_RANGE_MAX, ABS_RANGE_MAX)
        );
    }

    #[test]
    fn accumulates_scroll_pixels_into_whole_detents() {
        let mut remainder = 0.0;
        assert_eq!(take_scroll_detents(&mut remainder, false), 0);
        remainder += 10.0;
        assert_eq!(take_scroll_detents(&mut remainder, false), 0);
        remainder += 20.0;
        assert_eq!(take_scroll_detents(&mut remainder, false), 1);
        assert!(remainder.abs() < SCROLL_PIXELS_PER_DETENT);
    }

    #[test]
    fn flushes_a_partial_detent_when_the_gesture_finishes() {
        let mut remainder = 6.0;
        assert_eq!(take_scroll_detents(&mut remainder, true), 1);
        assert_eq!(remainder, 0.0);
        let mut negative = -6.0;
        assert_eq!(take_scroll_detents(&mut negative, true), -1);
    }

    #[test]
    fn maps_letters_digits_and_shifted_punctuation() {
        assert_eq!(char_to_key('a'), Some(plain(30)));
        assert_eq!(char_to_key('A'), Some(shifted(30)));
        assert_eq!(char_to_key('q'), Some(plain(16)));
        assert_eq!(char_to_key('m'), Some(plain(50)));
        assert_eq!(char_to_key('1'), Some(plain(2)));
        assert_eq!(char_to_key('0'), Some(plain(11)));
        assert_eq!(char_to_key('!'), Some(shifted(2)));
        assert_eq!(char_to_key('?'), Some(shifted(KEY_SLASH)));
        assert_eq!(char_to_key('é'), None);
    }

    #[test]
    fn maps_named_keysyms_and_function_keys() {
        assert_eq!(keysym_to_key(0xff0d), Some(plain(KEY_ENTER)));
        assert_eq!(keysym_to_key(0xff1b), Some(plain(KEY_ESC)));
        assert_eq!(keysym_to_key(0xffbe), Some(plain(KEY_F1)));
        assert_eq!(keysym_to_key(0xffc7), Some(plain(KEY_F1 + 9)));
        assert_eq!(keysym_to_key(0xffc8), Some(plain(KEY_F11)));
        assert_eq!(keysym_to_key(0xffc9), Some(plain(KEY_F12)));
        // Printable keysyms fall through to the character map.
        assert_eq!(keysym_to_key(0x41), Some(shifted(30)));
        assert_eq!(keysym_to_key(0x20), Some(plain(KEY_SPACE)));
    }
}
