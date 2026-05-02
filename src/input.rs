use crate::{
    capability::Capabilities,
    platform::{command_exists, hyprland_ipc_socket_path},
    protocol::{ButtonState, PointerButton},
};
use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[derive(Debug)]
pub enum InputManager {
    Noop { reason: String },
    Portal(WaylandPortalInputBackend),
    Hyprland(HyprlandHyprctlInputBackend),
}

impl InputManager {
    pub async fn from_capabilities(capabilities: &Capabilities) -> Self {
        match capabilities.input.backend.as_str() {
            "wayland-portal" => {}
            "hyprland-ipc" | "hyprland-hyprctl" => {
                return match HyprlandHyprctlInputBackend::new().await {
                    Ok(backend) => Self::Hyprland(backend),
                    Err(err) => Self::Noop {
                        reason: format!("Hyprland IPC input fallback unavailable: {err}"),
                    },
                };
            }
            _ => {
                return Self::Noop {
                    reason: capabilities
                        .input
                        .reason
                        .clone()
                        .unwrap_or_else(|| "Remote input unsupported on this host".into()),
                };
            }
        }
        match WaylandPortalInputBackend::new().await {
            Ok(backend) => Self::Portal(backend),
            Err(err) => Self::Noop {
                reason: format!("Remote input unavailable: {err}"),
            },
        }
    }

    pub async fn prepare(&mut self) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.prepare().await,
            Self::Hyprland(backend) => backend.prepare().await,
        }
    }

    pub async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.pointer_move(dx, dy).await,
            Self::Hyprland(backend) => backend.pointer_move(dx, dy).await,
        }
    }

    pub async fn pointer_move_absolute(&self, x: f64, y: f64) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.pointer_move_absolute(x, y).await,
            Self::Hyprland(backend) => backend.pointer_move_absolute(x, y).await,
        }
    }

    pub async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.pointer_button(button, state).await,
            Self::Hyprland(backend) => backend.pointer_button(button, state).await,
        }
    }

    pub async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.scroll(dx, dy, finish).await,
            Self::Hyprland(backend) => backend.scroll(dx, dy, finish).await,
        }
    }

    pub async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.key(keysym, state).await,
            Self::Hyprland(backend) => backend.key(keysym, state).await,
        }
    }

    pub async fn text(&self, text: &str) -> anyhow::Result<()> {
        match self {
            Self::Noop { reason } => bail!("{reason}"),
            Self::Portal(backend) => backend.text(text).await,
            Self::Hyprland(backend) => backend.text(text).await,
        }
    }
}

#[derive(Debug)]
pub struct HyprlandHyprctlInputBackend {
    socket_path: PathBuf,
    state: Arc<TokioMutex<HyprlandState>>,
    pointer_notify: Arc<Notify>,
    pointer_idle_notify: Arc<Notify>,
}

#[derive(Debug)]
struct HyprlandState {
    cursor: CursorPosition,
    cursor_x: f64,
    cursor_y: f64,
    pending_cursor: Option<CursorPosition>,
    in_flight_cursor: Option<CursorPosition>,
    pointer_in_flight: bool,
    pointer_last_error: Option<String>,
    scroll_x_remainder: f64,
    scroll_y_remainder: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
struct CursorPosition {
    x: i64,
    y: i64,
}

impl HyprlandHyprctlInputBackend {
    pub async fn new() -> anyhow::Result<Self> {
        let socket_path =
            hyprland_ipc_socket_path().context("HYPRLAND_INSTANCE_SIGNATURE is not set")?;
        let cursor: CursorPosition =
            serde_json::from_str(&hyprland_ipc_command(&socket_path, "j/cursorpos").await?)?;
        let state = Arc::new(TokioMutex::new(HyprlandState {
            cursor_x: cursor.x as f64,
            cursor_y: cursor.y as f64,
            cursor,
            pending_cursor: None,
            in_flight_cursor: None,
            pointer_in_flight: false,
            pointer_last_error: None,
            scroll_x_remainder: 0.0,
            scroll_y_remainder: 0.0,
        }));
        let pointer_notify = Arc::new(Notify::new());
        let pointer_idle_notify = Arc::new(Notify::new());
        tokio::spawn(run_hyprland_pointer_worker(
            socket_path.clone(),
            state.clone(),
            pointer_notify.clone(),
            pointer_idle_notify.clone(),
        ));
        Ok(Self {
            socket_path,
            state,
            pointer_notify,
            pointer_idle_notify,
        })
    }

    pub async fn prepare(&self) -> anyhow::Result<serde_json::Value> {
        Ok(json!({
            "backend": "hyprland-ipc",
            "status": "ready",
            "limitations": "uses Hyprland IPC because RemoteDesktop portal is unavailable; ASCII text is injected as key events and unsupported text falls back to clipboard paste"
        }))
    }

    pub async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        if let Some(error) = state.pointer_last_error.take() {
            bail!("Hyprland pointer dispatch failed: {error}");
        }
        state.cursor_x = clamp_cursor_f64(state.cursor_x + dx);
        state.cursor_y = clamp_cursor_f64(state.cursor_y + dy);
        let x = state.cursor_x.round() as i64;
        let y = state.cursor_y.round() as i64;
        let target = CursorPosition { x, y };
        let latest = state
            .pending_cursor
            .or(state.in_flight_cursor)
            .unwrap_or(state.cursor);
        if latest == target {
            return Ok(());
        }
        state.pending_cursor = Some(target);
        drop(state);
        self.pointer_notify.notify_one();
        Ok(())
    }

    pub async fn pointer_move_absolute(&self, x: f64, y: f64) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        if let Some(error) = state.pointer_last_error.take() {
            bail!("Hyprland pointer dispatch failed: {error}");
        }
        state.cursor_x = clamp_cursor_f64(x);
        state.cursor_y = clamp_cursor_f64(y);
        let target = CursorPosition {
            x: state.cursor_x.round() as i64,
            y: state.cursor_y.round() as i64,
        };
        let latest = state
            .pending_cursor
            .or(state.in_flight_cursor)
            .unwrap_or(state.cursor);
        if latest == target {
            return Ok(());
        }
        state.pending_cursor = Some(target);
        drop(state);
        self.pointer_notify.notify_one();
        Ok(())
    }

    pub async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> anyhow::Result<()> {
        self.flush_pointer().await?;
        let key = button.hyprland_key();
        let state = state.hyprland_state();
        self.dispatch(&format!("sendkeystate , {key},{state},activewindow"))
            .await
    }

    pub async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        self.flush_pointer().await?;
        let mut state = self.state.lock().await;
        state.scroll_x_remainder += dx;
        state.scroll_y_remainder += dy;
        let horizontal = scroll_steps(&mut state.scroll_x_remainder, finish);
        let vertical = scroll_steps(&mut state.scroll_y_remainder, finish);
        drop(state);

        for _ in 0..vertical.unsigned_abs().min(12) {
            self.send_shortcut(
                "",
                if vertical > 0 {
                    "mouse_down"
                } else {
                    "mouse_up"
                },
            )
            .await?;
        }
        for _ in 0..horizontal.unsigned_abs().min(12) {
            self.send_shortcut(
                "SHIFT",
                if horizontal > 0 {
                    "mouse_down"
                } else {
                    "mouse_up"
                },
            )
            .await?;
        }
        Ok(())
    }

    pub async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        self.flush_pointer().await?;
        let key = keysym_to_hyprland_key(keysym)?;
        self.dispatch(&format!(
            "sendkeystate , {key},{},activewindow",
            state.hyprland_state()
        ))
        .await
    }

    pub async fn text(&self, text: &str) -> anyhow::Result<()> {
        self.flush_pointer().await?;
        if text.is_empty() {
            return Ok(());
        }
        if text.len() > 4096 {
            bail!("Text input rejected: maximum length is 4096 bytes");
        }
        if self.send_text_as_key_events(text).await? {
            return Ok(());
        }
        self.paste_text_via_clipboard(text).await
    }

    async fn send_text_as_key_events(&self, text: &str) -> anyhow::Result<bool> {
        let shortcuts: Option<Vec<_>> = text.chars().map(text_char_to_hyprland_shortcut).collect();
        let Some(shortcuts) = shortcuts else {
            return Ok(false);
        };
        for shortcut in shortcuts {
            self.send_shortcut(shortcut.mods, shortcut.key).await?;
        }
        Ok(true)
    }

    async fn paste_text_via_clipboard(&self, text: &str) -> anyhow::Result<()> {
        if !command_exists("wl-copy") {
            bail!("Hyprland text input fallback requires wl-copy from wl-clipboard");
        }
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn wl-copy for Hyprland text input")?;
        let mut stdin = child.stdin.take().context("wl-copy stdin unavailable")?;
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
        let status = child.wait().await?;
        if !status.success() {
            bail!("wl-copy exited with {status}");
        }
        self.send_shortcut("CTRL", "V").await
    }

    async fn dispatch(&self, args: &str) -> anyhow::Result<()> {
        hyprland_dispatch(&self.socket_path, args).await
    }

    async fn flush_pointer(&self) -> anyhow::Result<()> {
        loop {
            let notified = self.pointer_idle_notify.notified();
            {
                let mut state = self.state.lock().await;
                if let Some(error) = state.pointer_last_error.take() {
                    bail!("Hyprland pointer dispatch failed: {error}");
                }
                if state.pending_cursor.is_none() && !state.pointer_in_flight {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    async fn send_shortcut(&self, mods: &str, key: &str) -> anyhow::Result<()> {
        self.dispatch(&format!("sendshortcut {mods}, {key},activewindow"))
            .await
    }
}

async fn run_hyprland_pointer_worker(
    socket_path: PathBuf,
    state: Arc<TokioMutex<HyprlandState>>,
    pointer_notify: Arc<Notify>,
    pointer_idle_notify: Arc<Notify>,
) {
    loop {
        pointer_notify.notified().await;
        loop {
            let target = {
                let mut state = state.lock().await;
                let Some(target) = state.pending_cursor.take() else {
                    state.pointer_in_flight = false;
                    state.in_flight_cursor = None;
                    pointer_idle_notify.notify_one();
                    break;
                };
                state.pointer_in_flight = true;
                state.in_flight_cursor = Some(target);
                target
            };

            let result = hyprland_dispatch(
                &socket_path,
                &format!("movecursor {} {}", target.x, target.y),
            )
            .await;
            let mut state = state.lock().await;
            match result {
                Ok(()) => {
                    state.cursor = target;
                    state.pointer_last_error = None;
                }
                Err(err) => {
                    state.pointer_last_error = Some(err.to_string());
                }
            }
            state.pointer_in_flight = false;
            state.in_flight_cursor = None;
            pointer_idle_notify.notify_one();
            if state.pending_cursor.is_none() {
                break;
            }
        }
    }
}

async fn hyprland_ipc_command(socket_path: &PathBuf, command: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to Hyprland IPC socket at {socket_path:?}"))?;
    stream.write_all(command.as_bytes()).await?;
    stream.shutdown().await?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .context("failed to read Hyprland IPC response")?;
    Ok(response)
}

async fn hyprland_dispatch(socket_path: &PathBuf, args: &str) -> anyhow::Result<()> {
    let response = hyprland_ipc_command(socket_path, &format!("dispatch {args}")).await?;
    if response.trim() == "ok" {
        Ok(())
    } else {
        bail!("Hyprland dispatch failed: {}", response.trim())
    }
}

fn scroll_steps(remainder: &mut f64, finish: bool) -> i32 {
    const THRESHOLD: f64 = 24.0;
    if finish && remainder.abs() > 3.0 {
        let direction = if *remainder > 0.0 { 1 } else { -1 };
        *remainder = 0.0;
        return direction;
    }
    let steps = (*remainder / THRESHOLD).trunc() as i32;
    *remainder -= steps as f64 * THRESHOLD;
    steps
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HyprlandShortcut {
    mods: &'static str,
    key: &'static str,
}

fn text_char_to_hyprland_shortcut(ch: char) -> Option<HyprlandShortcut> {
    let shortcut = match ch {
        'a'..='z' => HyprlandShortcut {
            mods: "",
            key: ascii_letter_key(ch),
        },
        'A'..='Z' => HyprlandShortcut {
            mods: "SHIFT",
            key: ascii_letter_key(ch),
        },
        '0'..='9' => HyprlandShortcut {
            mods: "",
            key: ascii_digit_key(ch),
        },
        ' ' => HyprlandShortcut {
            mods: "",
            key: "Space",
        },
        '\n' | '\r' => HyprlandShortcut {
            mods: "",
            key: "RETURN",
        },
        '\t' => HyprlandShortcut {
            mods: "",
            key: "Tab",
        },
        '!' => HyprlandShortcut {
            mods: "SHIFT",
            key: "1",
        },
        '@' => HyprlandShortcut {
            mods: "SHIFT",
            key: "2",
        },
        '#' => HyprlandShortcut {
            mods: "SHIFT",
            key: "3",
        },
        '$' => HyprlandShortcut {
            mods: "SHIFT",
            key: "4",
        },
        '%' => HyprlandShortcut {
            mods: "SHIFT",
            key: "5",
        },
        '^' => HyprlandShortcut {
            mods: "SHIFT",
            key: "6",
        },
        '&' => HyprlandShortcut {
            mods: "SHIFT",
            key: "7",
        },
        '*' => HyprlandShortcut {
            mods: "SHIFT",
            key: "8",
        },
        '(' => HyprlandShortcut {
            mods: "SHIFT",
            key: "9",
        },
        ')' => HyprlandShortcut {
            mods: "SHIFT",
            key: "0",
        },
        '-' => HyprlandShortcut {
            mods: "",
            key: "minus",
        },
        '_' => HyprlandShortcut {
            mods: "SHIFT",
            key: "minus",
        },
        '=' => HyprlandShortcut {
            mods: "",
            key: "equal",
        },
        '+' => HyprlandShortcut {
            mods: "SHIFT",
            key: "equal",
        },
        '[' => HyprlandShortcut {
            mods: "",
            key: "bracketleft",
        },
        '{' => HyprlandShortcut {
            mods: "SHIFT",
            key: "bracketleft",
        },
        ']' => HyprlandShortcut {
            mods: "",
            key: "bracketright",
        },
        '}' => HyprlandShortcut {
            mods: "SHIFT",
            key: "bracketright",
        },
        ';' => HyprlandShortcut {
            mods: "",
            key: "semicolon",
        },
        ':' => HyprlandShortcut {
            mods: "SHIFT",
            key: "semicolon",
        },
        '\'' => HyprlandShortcut {
            mods: "",
            key: "apostrophe",
        },
        '"' => HyprlandShortcut {
            mods: "SHIFT",
            key: "apostrophe",
        },
        ',' => HyprlandShortcut {
            mods: "",
            key: "comma",
        },
        '<' => HyprlandShortcut {
            mods: "SHIFT",
            key: "comma",
        },
        '.' => HyprlandShortcut {
            mods: "",
            key: "period",
        },
        '>' => HyprlandShortcut {
            mods: "SHIFT",
            key: "period",
        },
        '/' => HyprlandShortcut {
            mods: "",
            key: "slash",
        },
        '?' => HyprlandShortcut {
            mods: "SHIFT",
            key: "slash",
        },
        '\\' => HyprlandShortcut {
            mods: "",
            key: "backslash",
        },
        '|' => HyprlandShortcut {
            mods: "SHIFT",
            key: "backslash",
        },
        '`' => HyprlandShortcut {
            mods: "",
            key: "grave",
        },
        '~' => HyprlandShortcut {
            mods: "SHIFT",
            key: "grave",
        },
        _ => return None,
    };
    Some(shortcut)
}

fn ascii_letter_key(ch: char) -> &'static str {
    match ch.to_ascii_uppercase() {
        'A' => "A",
        'B' => "B",
        'C' => "C",
        'D' => "D",
        'E' => "E",
        'F' => "F",
        'G' => "G",
        'H' => "H",
        'I' => "I",
        'J' => "J",
        'K' => "K",
        'L' => "L",
        'M' => "M",
        'N' => "N",
        'O' => "O",
        'P' => "P",
        'Q' => "Q",
        'R' => "R",
        'S' => "S",
        'T' => "T",
        'U' => "U",
        'V' => "V",
        'W' => "W",
        'X' => "X",
        'Y' => "Y",
        'Z' => "Z",
        _ => unreachable!("caller checked ASCII letter"),
    }
}

fn ascii_digit_key(ch: char) -> &'static str {
    match ch {
        '0' => "0",
        '1' => "1",
        '2' => "2",
        '3' => "3",
        '4' => "4",
        '5' => "5",
        '6' => "6",
        '7' => "7",
        '8' => "8",
        '9' => "9",
        _ => unreachable!("caller checked ASCII digit"),
    }
}

fn keysym_to_hyprland_key(keysym: u32) -> anyhow::Result<&'static str> {
    let key = match keysym {
        0xffe3 | 0xffe4 => "Ctrl_L",
        0xffe9 | 0xffea => "Alt_L",
        0xffe1 | 0xffe2 => "Shift_L",
        0xffeb | 0xffec => "Super_L",
        0xff0d => "RETURN",
        0xff1b => "Escape",
        0xff09 => "Tab",
        0x0020 => "Space",
        0xff08 => "BackSpace",
        0xffff => "Delete",
        0xff51 => "left",
        0xff52 => "up",
        0xff53 => "right",
        0xff54 => "down",
        0x30 => "0",
        0x31 => "1",
        0x32 => "2",
        0x33 => "3",
        0x34 => "4",
        0x35 => "5",
        0x36 => "6",
        0x37 => "7",
        0x38 => "8",
        0x39 => "9",
        0x61 | 0x41 => "A",
        0x62 | 0x42 => "B",
        0x63 | 0x43 => "C",
        0x64 | 0x44 => "D",
        0x65 | 0x45 => "E",
        0x66 | 0x46 => "F",
        0x67 | 0x47 => "G",
        0x68 | 0x48 => "H",
        0x69 | 0x49 => "I",
        0x6a | 0x4a => "J",
        0x6b | 0x4b => "K",
        0x6c | 0x4c => "L",
        0x6d | 0x4d => "M",
        0x6e | 0x4e => "N",
        0x6f | 0x4f => "O",
        0x70 | 0x50 => "P",
        0x71 | 0x51 => "Q",
        0x72 | 0x52 => "R",
        0x73 | 0x53 => "S",
        0x74 | 0x54 => "T",
        0x75 | 0x55 => "U",
        0x76 | 0x56 => "V",
        0x77 | 0x57 => "W",
        0x78 | 0x58 => "X",
        0x79 | 0x59 => "Y",
        0x7a | 0x5a => "Z",
        _ => bail!("Unsupported Hyprland key keysym: 0x{keysym:x}"),
    };
    Ok(key)
}

impl PointerButton {
    fn hyprland_key(&self) -> &'static str {
        match self {
            Self::Left => "mouse:272",
            Self::Right => "mouse:273",
            Self::Middle => "mouse:274",
        }
    }
}

impl ButtonState {
    fn hyprland_state(&self) -> &'static str {
        match self {
            Self::Pressed => "down",
            Self::Released => "up",
        }
    }
}

impl WaylandPortalInputBackend {
    pub async fn text(&self, text: &str) -> anyhow::Result<()> {
        if text.len() > 4096 {
            bail!("Text input rejected: maximum length is 4096 bytes");
        }
        for ch in text.chars() {
            let keysym = ch as u32;
            self.key(keysym, crate::protocol::ButtonState::Pressed)
                .await?;
            self.key(keysym, crate::protocol::ButtonState::Released)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HyprlandShortcut, scroll_steps, text_char_to_hyprland_shortcut};

    #[test]
    fn maps_ascii_text_to_hyprland_shortcuts() {
        assert_eq!(
            text_char_to_hyprland_shortcut('a'),
            Some(HyprlandShortcut { mods: "", key: "A" })
        );
        assert_eq!(
            text_char_to_hyprland_shortcut('A'),
            Some(HyprlandShortcut {
                mods: "SHIFT",
                key: "A"
            })
        );
        assert_eq!(
            text_char_to_hyprland_shortcut('?'),
            Some(HyprlandShortcut {
                mods: "SHIFT",
                key: "slash"
            })
        );
        assert_eq!(text_char_to_hyprland_shortcut('è'), None);
    }

    #[test]
    fn coalesces_scroll_delta_into_bounded_steps() {
        let mut remainder = 50.0;
        assert_eq!(scroll_steps(&mut remainder, false), 2);
        assert!((remainder - 2.0).abs() < f64::EPSILON);
        assert_eq!(scroll_steps(&mut remainder, true), 0);

        let mut remainder = -5.0;
        assert_eq!(scroll_steps(&mut remainder, true), -1);
        assert_eq!(remainder, 0.0);
    }
}

fn clamp_cursor_f64(value: f64) -> f64 {
    value.clamp(-100_000.0, 100_000.0)
}

#[derive(Debug)]
pub struct WaylandPortalInputBackend {
    connection: zbus::Connection,
    session_handle: Option<OwnedObjectPath>,
    granted_devices: u32,
}

impl WaylandPortalInputBackend {
    pub async fn new() -> anyhow::Result<Self> {
        let connection = zbus::Connection::session().await?;
        Ok(Self {
            connection,
            session_handle: None,
            granted_devices: 0,
        })
    }

    pub async fn prepare(&mut self) -> anyhow::Result<serde_json::Value> {
        if self.session_handle.is_some() {
            return Ok(json!({
                "backend": "wayland-portal",
                "status": "ready",
                "devices": self.granted_devices
            }));
        }

        let proxy = self.proxy().await?;
        let mut create_options = HashMap::<&str, OwnedValue>::new();
        create_options.insert(
            "handle_token",
            Value::from(format!("waypad_create_{}", unique_token())).try_into()?,
        );
        create_options.insert(
            "session_handle_token",
            Value::from(format!("waypad_session_{}", unique_token())).try_into()?,
        );
        let create_handle: OwnedObjectPath = proxy.call("CreateSession", &(create_options)).await?;
        let create_response = wait_request(&self.connection, &create_handle).await?;
        if create_response.response != 0 {
            bail!("Input injection requires portal approval on this session");
        }
        let session_handle_string = create_response
            .results
            .get("session_handle")
            .and_then(owned_value_to_string)
            .context("RemoteDesktop portal did not return a session handle")?;
        let session_handle = OwnedObjectPath::try_from(session_handle_string.as_str())?;

        let mut select_options = HashMap::<&str, OwnedValue>::new();
        select_options.insert("types", Value::from(1u32 | 2u32).try_into()?);
        select_options.insert("persist_mode", Value::from(1u32).try_into()?);
        select_options.insert(
            "handle_token",
            Value::from(format!("waypad_select_{}", unique_token())).try_into()?,
        );
        let select_handle: OwnedObjectPath = proxy
            .call("SelectDevices", &(&session_handle, select_options))
            .await?;
        let select_response = wait_request(&self.connection, &select_handle).await?;
        if select_response.response != 0 {
            bail!("RemoteDesktop device selection was denied by the portal");
        }

        let mut start_options = HashMap::<&str, OwnedValue>::new();
        start_options.insert(
            "handle_token",
            Value::from(format!("waypad_start_{}", unique_token())).try_into()?,
        );
        let start_handle: OwnedObjectPath = proxy
            .call("Start", &(&session_handle, "", start_options))
            .await?;
        let start_response = wait_request(&self.connection, &start_handle).await?;
        if start_response.response != 0 {
            bail!("RemoteDesktop portal approval was denied or cancelled");
        }
        let devices = start_response
            .results
            .get("devices")
            .and_then(owned_value_to_u32)
            .unwrap_or(0);
        if devices & (1 | 2) == 0 {
            bail!("RemoteDesktop portal approved no keyboard or pointer devices");
        }

        self.session_handle = Some(session_handle);
        self.granted_devices = devices;
        Ok(json!({
            "backend": "wayland-portal",
            "status": "ready",
            "devices": devices
        }))
    }

    pub async fn pointer_move(&self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>("NotifyPointerMotion", &(session, empty_options(), dx, dy))
            .await?;
        Ok(())
    }

    pub async fn pointer_move_absolute(&self, x: f64, y: f64) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>(
                "NotifyPointerMotionAbsolute",
                &(session, empty_options(), 0u32, x, y),
            )
            .await
            .context(
                "RemoteDesktop portal absolute pointer motion failed; this backend may require a shared portal stream id",
            )?;
        Ok(())
    }

    pub async fn pointer_button(
        &self,
        button: PointerButton,
        state: ButtonState,
    ) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>(
                "NotifyPointerButton",
                &(
                    session,
                    empty_options(),
                    button.evdev_code(),
                    state.portal_state(),
                ),
            )
            .await?;
        Ok(())
    }

    pub async fn scroll(&self, dx: f64, dy: f64, finish: bool) -> anyhow::Result<()> {
        let session = self.session()?;
        let mut options = HashMap::<&str, OwnedValue>::new();
        options.insert("finish", Value::from(finish).try_into()?);
        self.proxy()
            .await?
            .call::<_, _, ()>("NotifyPointerAxis", &(session, options, dx, dy))
            .await?;
        Ok(())
    }

    pub async fn key(&self, keysym: u32, state: ButtonState) -> anyhow::Result<()> {
        let session = self.session()?;
        self.proxy()
            .await?
            .call::<_, _, ()>(
                "NotifyKeyboardKeysym",
                &(
                    session,
                    empty_options(),
                    keysym as i32,
                    state.portal_state(),
                ),
            )
            .await?;
        Ok(())
    }

    async fn proxy(&self) -> anyhow::Result<zbus::Proxy<'_>> {
        Ok(zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.RemoteDesktop",
        )
        .await?)
    }

    fn session(&self) -> anyhow::Result<&OwnedObjectPath> {
        self.session_handle
            .as_ref()
            .context("Input injection requires portal approval on this session")
    }
}

#[derive(Debug)]
struct PortalResponse {
    response: u32,
    results: HashMap<String, OwnedValue>,
}

async fn wait_request(
    connection: &zbus::Connection,
    handle: &OwnedObjectPath,
) -> anyhow::Result<PortalResponse> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        handle.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut stream = proxy.receive_signal("Response").await?;
    let message = timeout(Duration::from_secs(120), stream.next())
        .await
        .context("Timed out waiting for portal response")?
        .context("Portal request signal stream ended")?;
    let body = message.body();
    let (response, results): (u32, HashMap<String, OwnedValue>) = body.deserialize()?;
    Ok(PortalResponse { response, results })
}

fn empty_options() -> HashMap<&'static str, OwnedValue> {
    HashMap::new()
}

fn unique_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn owned_value_to_string(value: &OwnedValue) -> Option<String> {
    <&str>::try_from(value).map(|s| s.to_string()).ok()
}

fn owned_value_to_u32(value: &OwnedValue) -> Option<u32> {
    u32::try_from(value).ok()
}
