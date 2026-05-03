use crate::protocol::ButtonState;
use anyhow::{Context, bail};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    mem::{self, size_of},
    os::fd::AsRawFd,
    path::Path,
};
use tracing::{debug, info, warn};

const UINPUT_PATH: &str = "/dev/uinput";
const UINPUT_IOCTL_BASE: u8 = b'U';
const UINPUT_MAX_NAME_SIZE: usize = 80;
const ABS_CNT: usize = 0x40;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;

const BUS_USB: u16 = 0x03;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

const BTN_SOUTH: u16 = 0x130;
const BTN_EAST: u16 = 0x131;
const BTN_C: u16 = 0x132;
const BTN_NORTH: u16 = 0x133;
const BTN_WEST: u16 = 0x134;
const BTN_Z: u16 = 0x135;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_TL2: u16 = 0x138;
const BTN_TR2: u16 = 0x139;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;
const BTN_DPAD_UP: u16 = 0x220;
const BTN_DPAD_DOWN: u16 = 0x221;
const BTN_DPAD_LEFT: u16 = 0x222;
const BTN_DPAD_RIGHT: u16 = 0x223;

const STICK_MAX: i32 = 32_767;

const BUTTONS: &[(&str, u16)] = &[
    ("a", BTN_SOUTH),
    ("b", BTN_EAST),
    ("x", BTN_WEST),
    ("y", BTN_NORTH),
    ("c", BTN_C),
    ("z", BTN_Z),
    ("left_shoulder", BTN_TL),
    ("right_shoulder", BTN_TR),
    ("left_trigger_button", BTN_TL2),
    ("right_trigger_button", BTN_TR2),
    ("select", BTN_SELECT),
    ("start", BTN_START),
    ("mode", BTN_MODE),
    ("left_stick", BTN_THUMBL),
    ("right_stick", BTN_THUMBR),
    ("dpad_up", BTN_DPAD_UP),
    ("dpad_down", BTN_DPAD_DOWN),
    ("dpad_left", BTN_DPAD_LEFT),
    ("dpad_right", BTN_DPAD_RIGHT),
];

const AXES: &[(&str, u16)] = &[
    ("left_x", ABS_X),
    ("left_y", ABS_Y),
    ("right_x", ABS_RX),
    ("right_y", ABS_RY),
    ("left_trigger", ABS_Z),
    ("right_trigger", ABS_RZ),
    ("hat_x", ABS_HAT0X),
    ("hat_y", ABS_HAT0Y),
];

#[derive(Debug)]
pub struct ControllerInputManager {
    available: bool,
    unavailable_reason: String,
    backend: Option<VirtualGamepadBackend>,
}

impl ControllerInputManager {
    pub fn new(available: bool, reason: impl Into<String>) -> Self {
        Self {
            available,
            unavailable_reason: reason.into(),
            backend: None,
        }
    }

    pub fn refresh(&mut self, available: bool, reason: impl Into<String>) {
        self.available = available;
        self.unavailable_reason = reason.into();
        if !self.available {
            self.backend = None;
        }
    }

    pub fn device_connected(&mut self, device_id: &str, name: &str) -> anyhow::Result<()> {
        let backend = self.ensure_backend()?;
        info!(%device_id, %name, "android controller attached to virtual gamepad");
        backend.reset()
    }

    pub fn device_disconnected(&mut self, device_id: &str) -> anyhow::Result<()> {
        if let Some(backend) = &mut self.backend {
            info!(%device_id, "android controller detached from virtual gamepad");
            backend.reset()?;
        }
        Ok(())
    }

    pub fn button(&mut self, button: &str, state: ButtonState) -> anyhow::Result<()> {
        self.ensure_backend()?.button(button, state)
    }

    pub fn axis(&mut self, axis: &str, value: f64) -> anyhow::Result<()> {
        self.ensure_backend()?.axis(axis, value)
    }

    pub fn flush_pending(&mut self) -> anyhow::Result<()> {
        if let Some(backend) = &mut self.backend {
            backend.flush_pending()
        } else {
            Ok(())
        }
    }

    fn ensure_backend(&mut self) -> anyhow::Result<&mut VirtualGamepadBackend> {
        if !self.available {
            bail!("{}", self.unavailable_reason);
        }
        if self.backend.is_none() {
            self.backend = Some(VirtualGamepadBackend::create()?);
        }
        Ok(self.backend.as_mut().expect("backend just initialized"))
    }
}

pub fn detect_virtual_gamepad_support() -> (bool, String) {
    if !Path::new(UINPUT_PATH).exists() {
        return (
            false,
            "Controller forwarding requires /dev/uinput. Load the uinput module and allow the Waypad user to open the device."
                .into(),
        );
    }
    match OpenOptions::new().write(true).open(UINPUT_PATH) {
        Ok(_) => (
            true,
            "Controller events can be forwarded through a Linux uinput virtual gamepad.".into(),
        ),
        Err(err) => (
            false,
            format!(
                "Controller forwarding requires write access to {UINPUT_PATH}; current user cannot open it: {err}"
            ),
        ),
    }
}

#[derive(Debug)]
pub struct VirtualGamepadBackend {
    file: File,
    button_state: HashMap<u16, i32>,
    axis_state: HashMap<u16, i32>,
    dirty: bool,
}

impl VirtualGamepadBackend {
    fn create() -> anyhow::Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(UINPUT_PATH)
            .with_context(|| format!("opening {UINPUT_PATH}"))?;

        set_evbit(&file, EV_KEY)?;
        set_evbit(&file, EV_ABS)?;
        for (_, code) in BUTTONS {
            set_keybit(&file, *code)?;
        }
        for (_, code) in AXES {
            set_absbit(&file, *code)?;
        }

        let mut device = UInputUserDev::named("Waypad Android Virtual Gamepad");
        device.id.bustype = BUS_USB;
        device.id.vendor = 0x1209;
        device.id.product = 0x5750;
        device.id.version = 1;
        configure_axis(&mut device, ABS_X, -STICK_MAX, STICK_MAX, 4_096, 0);
        configure_axis(&mut device, ABS_Y, -STICK_MAX, STICK_MAX, 4_096, 0);
        configure_axis(&mut device, ABS_RX, -STICK_MAX, STICK_MAX, 4_096, 0);
        configure_axis(&mut device, ABS_RY, -STICK_MAX, STICK_MAX, 4_096, 0);
        configure_axis(&mut device, ABS_Z, 0, STICK_MAX, 0, 0);
        configure_axis(&mut device, ABS_RZ, 0, STICK_MAX, 0, 0);
        configure_axis(&mut device, ABS_HAT0X, -1, 1, 0, 0);
        configure_axis(&mut device, ABS_HAT0Y, -1, 1, 0, 0);

        write_struct(&mut file, &device).context("writing uinput device setup")?;
        ioctl_noarg(&file, ui_dev_create()).context("creating uinput gamepad")?;

        info!("created Waypad uinput virtual gamepad");
        Ok(Self {
            file,
            button_state: HashMap::new(),
            axis_state: HashMap::new(),
            dirty: false,
        })
    }

    fn button(&mut self, button: &str, state: ButtonState) -> anyhow::Result<()> {
        let code = button_code(button)?;
        let value = match state {
            ButtonState::Pressed => 1,
            ButtonState::Released => 0,
        };
        if self.button_state.get(&code).copied() == Some(value) {
            return Ok(());
        }
        self.button_state.insert(code, value);
        debug!(button, code, value, "virtual gamepad button");
        self.emit(EV_KEY, code, value)?;
        self.sync()?;
        self.file.flush().context("flushing uinput event")
    }

    fn axis(&mut self, axis: &str, value: f64) -> anyhow::Result<()> {
        if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
            bail!("Controller axis value out of range for {axis}: {value}");
        }
        let code = axis_code(axis)?;
        let scaled = scale_axis(axis, value);
        if self.axis_state.get(&code).copied() == Some(scaled) {
            return Ok(());
        }
        self.axis_state.insert(code, scaled);
        debug!(axis, code, value, scaled, "virtual gamepad axis");
        self.emit(EV_ABS, code, scaled)?;
        self.sync()?;
        Ok(())
    }

    fn flush_pending(&mut self) -> anyhow::Result<()> {
        if self.dirty {
            self.dirty = false;
            self.file.flush().context("flushing uinput event batch")
        } else {
            Ok(())
        }
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        for (_, code) in BUTTONS {
            self.emit(EV_KEY, *code, 0)?;
            self.button_state.insert(*code, 0);
        }
        for (axis, code) in AXES {
            let value = scale_axis(axis, 0.0);
            self.emit(EV_ABS, *code, value)?;
            self.axis_state.insert(*code, value);
        }
        self.sync()
    }

    fn emit(&mut self, event_type: u16, code: u16, value: i32) -> anyhow::Result<()> {
        let event = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            event_type,
            code,
            value,
        };
        write_struct(&mut self.file, &event)
    }

    fn sync(&mut self) -> anyhow::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)?;
        self.dirty = true;
        Ok(())
    }
}

impl Drop for VirtualGamepadBackend {
    fn drop(&mut self) {
        if let Err(err) = self.reset() {
            warn!(%err, "failed to reset virtual gamepad before drop");
        }
        if let Err(err) = ioctl_noarg(&self.file, ui_dev_destroy()) {
            warn!(%err, "failed to destroy uinput virtual gamepad");
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UInputUserDev {
    name: [u8; UINPUT_MAX_NAME_SIZE],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; ABS_CNT],
    absmin: [i32; ABS_CNT],
    absfuzz: [i32; ABS_CNT],
    absflat: [i32; ABS_CNT],
}

impl UInputUserDev {
    fn named(name: &str) -> Self {
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
struct InputEvent {
    time: libc::timeval,
    event_type: u16,
    code: u16,
    value: i32,
}

fn configure_axis(device: &mut UInputUserDev, code: u16, min: i32, max: i32, flat: i32, fuzz: i32) {
    let index = code as usize;
    device.absmin[index] = min;
    device.absmax[index] = max;
    device.absflat[index] = flat;
    device.absfuzz[index] = fuzz;
}

fn write_struct<T>(file: &mut File, value: &T) -> anyhow::Result<()> {
    let bytes =
        unsafe { std::slice::from_raw_parts(value as *const T as *const u8, mem::size_of::<T>()) };
    file.write_all(bytes).map_err(Into::into)
}

fn set_evbit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_evbit(), code)
}

fn set_keybit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_keybit(), code)
}

fn set_absbit(file: &File, code: u16) -> anyhow::Result<()> {
    ioctl_int(file, ui_set_absbit(), code)
}

fn ioctl_int(file: &File, request: libc::c_ulong, value: u16) -> anyhow::Result<()> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, value as libc::c_int) };
    if result < 0 {
        Err(std::io::Error::last_os_error()).context("uinput ioctl failed")
    } else {
        Ok(())
    }
}

fn ioctl_noarg(file: &File, request: libc::c_ulong) -> anyhow::Result<()> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request) };
    if result < 0 {
        Err(std::io::Error::last_os_error()).context("uinput ioctl failed")
    } else {
        Ok(())
    }
}

fn ui_dev_create() -> libc::c_ulong {
    ioc(0, UINPUT_IOCTL_BASE, 1, 0)
}

fn ui_dev_destroy() -> libc::c_ulong {
    ioc(0, UINPUT_IOCTL_BASE, 2, 0)
}

fn ui_set_evbit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 100, size_of::<libc::c_int>())
}

fn ui_set_keybit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 101, size_of::<libc::c_int>())
}

fn ui_set_absbit() -> libc::c_ulong {
    iow(UINPUT_IOCTL_BASE, 103, size_of::<libc::c_int>())
}

fn iow(io_type: u8, nr: u8, size: usize) -> libc::c_ulong {
    ioc(1, io_type, nr, size)
}

fn ioc(dir: u8, io_type: u8, nr: u8, size: usize) -> libc::c_ulong {
    const NRSHIFT: u32 = 0;
    const TYPESHIFT: u32 = 8;
    const SIZESHIFT: u32 = 16;
    const DIRSHIFT: u32 = 30;
    ((dir as libc::c_ulong) << DIRSHIFT)
        | ((io_type as libc::c_ulong) << TYPESHIFT)
        | ((nr as libc::c_ulong) << NRSHIFT)
        | ((size as libc::c_ulong) << SIZESHIFT)
}

fn button_code(button: &str) -> anyhow::Result<u16> {
    BUTTONS
        .iter()
        .find_map(|(name, code)| (*name == button).then_some(*code))
        .with_context(|| format!("Unsupported controller button: {button}"))
}

fn axis_code(axis: &str) -> anyhow::Result<u16> {
    AXES.iter()
        .find_map(|(name, code)| (*name == axis).then_some(*code))
        .with_context(|| format!("Unsupported controller axis: {axis}"))
}

pub fn scale_axis(axis: &str, value: f64) -> i32 {
    let clamped = value.clamp(-1.0, 1.0);
    match axis {
        "left_trigger" | "right_trigger" => {
            (clamped.max(0.0) * f64::from(STICK_MAX)).round() as i32
        }
        "hat_x" | "hat_y" => clamped.round() as i32,
        _ => (clamped * f64::from(STICK_MAX)).round() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_android_controller_buttons_to_linux_gamepad_buttons() {
        assert_eq!(button_code("a").unwrap(), BTN_SOUTH);
        assert_eq!(button_code("b").unwrap(), BTN_EAST);
        assert_eq!(button_code("dpad_left").unwrap(), BTN_DPAD_LEFT);
        assert!(button_code("unknown").is_err());
    }

    #[test]
    fn maps_android_controller_axes_to_linux_abs_axes() {
        assert_eq!(axis_code("left_x").unwrap(), ABS_X);
        assert_eq!(axis_code("right_y").unwrap(), ABS_RY);
        assert_eq!(axis_code("right_trigger").unwrap(), ABS_RZ);
        assert!(axis_code("gyro_x").is_err());
    }

    #[test]
    fn scales_axis_values_for_uinput_ranges() {
        assert_eq!(scale_axis("left_x", -1.0), -STICK_MAX);
        assert_eq!(scale_axis("left_x", 1.0), STICK_MAX);
        assert_eq!(scale_axis("left_trigger", -1.0), 0);
        assert_eq!(scale_axis("left_trigger", 1.0), STICK_MAX);
        assert_eq!(scale_axis("hat_x", -0.8), -1);
    }
}
