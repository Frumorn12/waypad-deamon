use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u16 = 1;
pub const DISCOVERY_MAGIC: &[u8] = b"WAYPAD_DISCOVER_V1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientPlain {
    ClientHello {
        protocol: u16,
        client_ephemeral_pub: String,
        device_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerPlain {
    ServerHello {
        protocol: u16,
        host_public_key: String,
        host_fingerprint: String,
        server_ephemeral_pub: String,
        signature: String,
        session_nonce: String,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientSecureMessage {
    PairRequest {
        request_id: String,
        device_name: String,
        pairing_code: String,
        app_version: Option<String>,
    },
    AuthRequest {
        request_id: String,
        device_id: String,
        session_token: String,
        app_version: Option<String>,
    },
    Command {
        request_id: String,
        command: Command,
    },
    Ping {
        request_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerSecureMessage {
    Response {
        request_id: String,
        ok: bool,
        data: Option<Value>,
        error: Option<ApiError>,
    },
    Event {
        name: String,
        data: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum Command {
    GetHealth,
    GetHostInfo,
    GetCapabilities,
    PrepareInput,
    PointerMove {
        dx: f64,
        dy: f64,
    },
    PointerMoveAbsolute {
        source_id: Option<String>,
        x: f64,
        y: f64,
    },
    PointerButton {
        button: PointerButton,
        state: ButtonState,
    },
    Scroll {
        dx: f64,
        dy: f64,
        finish: bool,
    },
    ExternalInput {
        device_id: String,
        device_type: ExternalDeviceType,
        event: ExternalInputEvent,
    },
    Key {
        keysym: u32,
        state: ButtonState,
    },
    Text {
        text: String,
    },
    Shortcut {
        keys: Vec<String>,
    },
    Media {
        action: MediaAction,
    },
    Volume {
        action: VolumeAction,
    },
    Brightness {
        action: BrightnessAction,
    },
    ClipboardSet {
        text: String,
    },
    ListScreenSources,
    StartScreenStream {
        source_id: Option<String>,
        max_fps: Option<u32>,
        jpeg_quality: Option<u8>,
        max_width: Option<u32>,
        max_height: Option<u32>,
    },
    StopScreenStream {
        session_id: String,
    },
    System {
        action: SystemAction,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Left,
    Middle,
    Right,
}

impl PointerButton {
    pub fn evdev_code(&self) -> i32 {
        match self {
            Self::Left => 272,
            Self::Right => 273,
            Self::Middle => 274,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonState {
    Pressed,
    Released,
}

impl ButtonState {
    pub fn portal_state(&self) -> u32 {
        match self {
            Self::Released => 0,
            Self::Pressed => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalDeviceType {
    Keyboard,
    Mouse,
    Touchpad,
    Gamepad,
    Joystick,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExternalInputEvent {
    DeviceConnected {
        name: String,
        classes: Vec<ExternalDeviceType>,
    },
    DeviceDisconnected,
    PointerMove {
        dx: f64,
        dy: f64,
    },
    PointerButton {
        button: PointerButton,
        state: ButtonState,
    },
    PointerScroll {
        dx: f64,
        dy: f64,
        finish: bool,
    },
    KeyboardKey {
        keysym: u32,
        state: ButtonState,
        repeat: bool,
    },
    ControllerButton {
        button: String,
        state: ButtonState,
    },
    ControllerAxis {
        axis: String,
        value: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeAction {
    Up,
    Down,
    MuteToggle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrightnessAction {
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemAction {
    Lock,
    Suspend,
}

pub fn response_ok(request_id: impl Into<String>, data: Value) -> ServerSecureMessage {
    ServerSecureMessage::Response {
        request_id: request_id.into(),
        ok: true,
        data: Some(data),
        error: None,
    }
}

pub fn response_empty(request_id: impl Into<String>) -> ServerSecureMessage {
    ServerSecureMessage::Response {
        request_id: request_id.into(),
        ok: true,
        data: None,
        error: None,
    }
}

pub fn response_error(request_id: impl Into<String>, error: ApiError) -> ServerSecureMessage {
    ServerSecureMessage::Response {
        request_id: request_id.into(),
        ok: false,
        data: None,
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trips_as_tagged_json() {
        let command = Command::PointerButton {
            button: PointerButton::Left,
            state: ButtonState::Pressed,
        };
        let raw = serde_json::to_string(&command).unwrap();
        assert!(raw.contains("pointer_button"));
        let decoded: Command = serde_json::from_str(&raw).unwrap();
        assert!(matches!(decoded, Command::PointerButton { .. }));
    }

    #[test]
    fn screen_commands_round_trip() {
        let command = Command::StartScreenStream {
            source_id: Some("hyprland:monitor:DP-1".into()),
            max_fps: Some(60),
            jpeg_quality: Some(58),
            max_width: Some(1280),
            max_height: Some(1280),
        };
        let raw = serde_json::to_string(&command).unwrap();
        assert!(raw.contains("start_screen_stream"));
        assert!(raw.contains("max_width"));
        let decoded: Command = serde_json::from_str(&raw).unwrap();
        assert!(matches!(decoded, Command::StartScreenStream { .. }));

        let absolute = Command::PointerMoveAbsolute {
            source_id: Some("hyprland:monitor:DP-1".into()),
            x: 100.0,
            y: 200.0,
        };
        let raw = serde_json::to_string(&absolute).unwrap();
        assert!(raw.contains("pointer_move_absolute"));
        let decoded: Command = serde_json::from_str(&raw).unwrap();
        assert!(matches!(decoded, Command::PointerMoveAbsolute { .. }));
    }

    #[test]
    fn external_input_command_round_trips() {
        let command = Command::ExternalInput {
            device_id: "android:1:abcd".into(),
            device_type: ExternalDeviceType::Mouse,
            event: ExternalInputEvent::PointerMove { dx: 4.0, dy: -2.0 },
        };
        let raw = serde_json::to_string(&command).unwrap();
        assert!(raw.contains("external_input"));
        assert!(raw.contains("pointer_move"));
        let decoded: Command = serde_json::from_str(&raw).unwrap();
        assert!(matches!(decoded, Command::ExternalInput { .. }));
    }
}
