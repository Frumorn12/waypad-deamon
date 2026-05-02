# Waypad Protocol

Waypad uses a custom low-latency TCP protocol instead of HTTP polling. TCP is used for reliable ordering; cryptographic framing is implemented at the application layer so the Android client can pin the daemon host key without a local TLS PKI.

Default ports:

| Purpose | Port | Protocol |
| --- | ---: | --- |
| Discovery | 47770 | UDP broadcast |
| Control | 47771 | TCP with encrypted Waypad frames |
| Screen stream | 47771 | Token-attached TCP frame stream on the control listener |

## Discovery

The Android app broadcasts:

```text
WAYPAD_DISCOVER_V1
```

The daemon replies with JSON:

```json
{
  "service": "dev.waypad.daemon",
  "protocol": 1,
  "host_name": "desktop",
  "control_port": 47771,
  "host_fingerprint": "abcd:...",
  "input_backend": "wayland-portal",
  "input_supported": true,
  "capture_backend": "wayland-screencast-portal",
  "capture_supported": true
}
```

Discovery is only a convenience mechanism. Trust is established by the signed TCP handshake and pairing code.

## Handshake

The client sends a plaintext `client_hello` containing an ephemeral P-256 ECDH public key. The daemon replies with:

- Daemon long-term P-256 ECDSA public key.
- Daemon host fingerprint, computed as SHA-256 over the host public key and grouped as colon-separated hex.
- Daemon ephemeral P-256 ECDH public key.
- 32-byte random session nonce.
- ECDSA signature over `WAYPAD-HANDSHAKE-v1 || client_ephemeral || server_ephemeral || session_nonce`.

The client verifies the signature and checks the fingerprint against discovery, QR/manual pairing data, or a previously pinned host.

## Encrypted Frames

Both sides derive keys with HKDF-SHA256:

- Salt: session nonce.
- Input key material: ECDH shared secret.
- Client-to-server info: `waypad v1 c2s`.
- Server-to-client info: `waypad v1 s2c`.

Frames are newline-delimited JSON:

```json
{
  "seq": 0,
  "ciphertext": "base64(aes-gcm-json)"
}
```

AES-GCM nonces are `C2S\0 || seq_u64_be` or `S2C\0 || seq_u64_be`. The sequence number is also authenticated as AEAD additional data. Receivers reject out-of-order frames.

## Pairing

Pairing requires a local code:

```bash
waypad-daemon pair-code
```

The app sends an encrypted `pair_request` with the code and Android device name. If valid, the daemon creates a trusted device, stores only a SHA-256 hash of the random 256-bit session token, and returns the token once.

## Authentication

The app sends an encrypted `auth_request` with:

```json
{
  "type": "auth_request",
  "request_id": "...",
  "device_id": "...",
  "session_token": "...",
  "app_version": "0.1.0"
}
```

The daemon rejects all control commands until authentication succeeds.

## Commands

Each command is encrypted:

```json
{
  "type": "command",
  "request_id": "...",
  "command": {
    "name": "pointer_move",
    "dx": 12.0,
    "dy": -3.5
  }
}
```

Current command names:

| Command | Purpose |
| --- | --- |
| `get_health` | Service health. |
| `get_host_info` | Host name, fingerprint, protocol. |
| `get_capabilities` | Wayland, portal, and system capability model. |
| `prepare_input` | Starts the RemoteDesktop portal approval flow. |
| `pointer_move` | Relative pointer motion. |
| `pointer_move_absolute` | Source-local absolute pointer motion for remote screen control. |
| `pointer_button` | Left, middle, or right button press/release. |
| `scroll` | Smooth pointer-axis scroll. |
| `external_input` | Normalized input events from Android-attached mouse, keyboard, touchpad, or controller devices. |
| `key` | XKB keysym press/release. |
| `text` | Sends characters as keysyms. |
| `shortcut` | Sends a validated shortcut sequence. |
| `media` | `playerctl` media actions. |
| `volume` | `wpctl` or `pactl` volume actions. |
| `brightness` | `brightnessctl` brightness actions. |
| `clipboard_set` | Sets Wayland clipboard via `wl-copy`. |
| `list_screen_sources` | Lists portal picker and/or concrete monitor sources. |
| `start_screen_stream` | Starts a token-protected screen frame stream. |
| `stop_screen_stream` | Stops a running screen stream session. |
| `system` | Lock or suspend. Suspend is disabled by default. |

Unsupported commands return an authenticated error with a user-facing reason.

## External Android Input

The Android client forwards hardware devices connected to the phone with:

```json
{
  "name": "external_input",
  "device_id": "android:7:abcd1234",
  "device_type": "keyboard",
  "event": {
    "type": "keyboard_key",
    "keysym": 97,
    "state": "pressed",
    "repeat": false
  }
}
```

`device_type` is one of `keyboard`, `mouse`, `touchpad`, `gamepad`, `joystick`, or `unknown`. Event types are:

| Event | Host behavior |
| --- | --- |
| `device_connected` / `device_disconnected` | Logged for diagnostics. |
| `pointer_move` | Relative pointer motion through the active backend. |
| `pointer_button` | Left/middle/right button through the active backend. |
| `pointer_scroll` | Smooth scroll through the active backend. |
| `keyboard_key` | XKB keysym press/release through the active backend. |
| `controller_button` / `controller_axis` | Sent to the Linux `uinput` virtual gamepad backend when `external_input.controller` is true. |

`get_capabilities` includes `external_input.pointer`, `external_input.keyboard`, and `external_input.controller`. Pointer and keyboard follow the current input backend. Controller support is independent and reflects whether the daemon user can open `/dev/uinput`; current Wayland portal and Hyprland IPC APIs do not provide generic gamepad injection themselves.

## Screen Sources

`list_screen_sources` returns:

```json
{
  "sources": [
    {
      "id": "hyprland:monitor:DP-1",
      "label": "DP-1 (monitor description)",
      "kind": "monitor",
      "backend": "hyprland-grim",
      "width": 1920,
      "height": 1080,
      "x": 0,
      "y": 0,
      "scale": 1.0,
      "focused": true
    }
  ]
}
```

When the standard portal path is available, the daemon also exposes `portal:chooser`. The actual monitor/window is selected locally through the compositor portal dialog.

## Screen Stream

The Android app starts a stream with:

```json
{
  "name": "start_screen_stream",
  "source_id": "hyprland:monitor:DP-1",
  "max_fps": 12,
  "jpeg_quality": 70
}
```

The daemon replies:

```json
{
  "session_id": "...",
  "stream_port": 47771,
  "token": "...",
  "codec": "jpeg",
  "transport": "waypad-control-port-stream-v2",
  "source": { "id": "hyprland:monitor:DP-1" }
}
```

For `waypad-control-port-stream-v2`, the app opens a fresh TCP connection to `stream_port` and writes this JSON line before any encrypted control-channel handshake:

```json
{"type":"stream_connect","token":"..."}
```

The daemon peeks at new TCP connections on the control listener. If the first line is a valid `stream_connect` token for a pending screen session, that socket is attached to the stream producer and receives:

```text
WAYPAD_STREAM_V1\n
```

Frames then repeat:

```text
u32_be header_length
u32_be payload_length
header_length bytes of UTF-8 JSON
payload_length bytes of JPEG
```

The frame header contains at least `seq`, `timestamp_ms`, `codec`, `width`, and `height`.

The current MVP stream is token-protected but not encrypted independently; it is intended for the same trusted LAN model as discovery/control pairing. The authenticated control channel remains encrypted. Older builds used a dynamic per-session stream port, but current builds reuse the stable control port so phone clients are not broken by LAN firewalls or NAT rules that block random high ports. A future WebRTC/H.264 transport can replace this frame stream while keeping the source and input commands.
