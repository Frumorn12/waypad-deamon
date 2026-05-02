# Architecture

Waypad-daemon is a user-session Linux daemon. It is intentionally not a root system service because Wayland input authorization, xdg-desktop-portal, and compositor consent are scoped to the logged-in graphical session.

## Components

| Module | Responsibility |
| --- | --- |
| `config` | JSON config loading and defaults. |
| `state` | Host identity, pairing code, trusted devices, private file permissions. |
| `crypto` | P-256 handshake, host signatures, HKDF, AES-GCM frame encryption. |
| `discovery` | UDP LAN discovery. |
| `server` | TCP listener, authentication, command routing, rate limiting. |
| `capability` | Session, portal, libei, connectivity, and system helper detection. |
| `input` | Wayland RemoteDesktop portal backend and unsupported fallback. |
| `screen` | Screen source enumeration, ScreenCast/PipeWire stream sessions, and Hyprland capture fallback. |
| `system_control` | Volume, media, brightness, clipboard, lock, suspend commands. |
| `platform` | Environment and compositor detection helpers. |

## Wayland Input Strategy

Waypad does not use `xdotool` or XTest because those are X11-era mechanisms and do not model Wayland security. The supported path is:

1. Detect a Wayland session.
2. Detect `org.freedesktop.portal.Desktop` on the session bus.
3. Detect `org.freedesktop.portal.RemoteDesktop`.
4. Request keyboard/pointer devices through the portal.
5. Wait for local user approval.
6. Send input through `NotifyPointer*` and `NotifyKeyboard*` portal methods.

If portal support is missing or approval is denied, input commands fail with explicit messages. This is expected on some Hyprland/wlroots setups depending on installed portal backend and version.

The daemon also detects whether RemoteDesktop version 2 may expose `ConnectToEIS`, and whether libei appears installed, but MVP input uses portal Notify methods. libei event sending is the next backend extension point.

When Hyprland is detected and the RemoteDesktop portal is missing, the daemon can use a `hyprland-ipc` backend. This talks to Hyprland's user-session IPC socket, not root/uinput, and is isolated behind the same `InputManager` abstraction as the portal backend. It supports cursor movement, mouse button state, scroll wheel events, shortcuts, and direct ASCII text events. Unsupported text falls back to `wl-copy` paste, so only that fallback path temporarily replaces the Wayland clipboard.

External Android mouse and keyboard devices use the same input abstraction as touchpad and remote-screen input. The protocol keeps Android device metadata and normalized event types, but host-side pointer and keyboard events still terminate in RemoteDesktop portal methods or the Hyprland IPC fallback.

Controller/gamepad forwarding uses a separate Linux `uinput` backend because current Wayland RemoteDesktop portal methods and the Hyprland IPC fallback do not expose a generic virtual gamepad injection API. When `/dev/uinput` is writable by the daemon user, controller buttons and axes are mapped to a standard "Waypad Android Virtual Gamepad" device using Linux `BTN_*` and `ABS_*` codes. This is intentionally isolated from pointer/keyboard injection so Wayland portal behavior remains compositor-scoped while gamepad support uses the kernel input API that browser Gamepad APIs already understand.

## Remote Screen Strategy

Remote screen support is intentionally Wayland-first:

1. Detect `org.freedesktop.portal.ScreenCast`.
2. Detect PipeWire runtime availability.
3. Detect a usable GStreamer `pipewiresrc ! jpegenc` pipeline.
4. Offer a portal source picker when the standard portal stack is usable.
5. On Hyprland only, offer an isolated `hyprland-grim` monitor fallback when portal streaming dependencies are incomplete.

The control channel negotiates a short-lived stream session and token. The Android app then opens a second LAN TCP connection to the daemon's stable control port, sends a `stream_connect` JSON line with that token, and receives `WAYPAD_STREAM_V1` frames. Each frame is a JSON header plus JPEG payload. Reusing the control listener avoids dynamic high-port failures on real phones and keeps the MVP small and shippable without adding a partial WebRTC stack. The transport is designed so a future WebRTC/H.264 backend can replace the frame stream without changing source selection or input mapping commands.

Stream settings are not cosmetic. Android sends `max_fps`, `jpeg_quality`,
`max_width`, and `max_height` in `start_screen_stream`. The daemon clamps FPS to
1..60, JPEG quality to 35..92, and maximum dimensions to 480..3840. Hyprland
`grim` capture uses the requested scale before JPEG encoding, and the
PipeWire/GStreamer path inserts `videorate`, `videoscale`, and leaky downstream
queues so stale frames are dropped instead of accumulating latency.

Portal capture requires local user approval. Hyprland `grim` capture is compositor-specific and deliberately isolated behind the `ScreenManager`; it is not treated as a general Wayland backend.

Absolute pointer control uses the existing input abstraction. Hyprland maps source-local coordinates to global compositor coordinates and dispatches `movecursor`. The RemoteDesktop portal path exposes absolute motion through `NotifyPointerMotionAbsolute`, but some portal backends require a shared screencast stream id; those failures are surfaced to the Android app instead of silently falling back to incorrect coordinates.

## Security Boundaries

The daemon binds to LAN by default but still treats the LAN as hostile:

- Unknown clients only receive a signed handshake.
- Pairing requires a local one-time code.
- Commands require authentication.
- Device tokens can be revoked.
- Host key rotation invalidates existing trust.
- Public internet source addresses are rejected when `require_private_lan` is enabled.

QR invites are expiring pairing helpers, not permanent credentials. The
`invite` command creates a normal one-time pairing code, embeds it in a
`waypad://invite` payload with the host fingerprint, address, port, route, and
expiry, and optionally prints it as an ANSI terminal QR. The Android app still
verifies the signed daemon handshake and pins the host key before trusting the
connection.

## Connectivity Model

Current builds support direct TCP connectivity:

- LAN direct through discovery, manual IP entry, or `waypad-daemon invite --qr`.
- Public direct through `waypad-daemon invite --qr --remote-address <host>` when
  the user intentionally exposes TCP `47771`.

The daemon reports this in `capabilities.connectivity`. It also explicitly
reports that relay, signaling, STUN, and TURN are not available. That keeps
outside-LAN behavior honest: port forwarding or a VPN can work today, but
automatic NAT traversal requires a future WebRTC/ICE/TURN backend.

## Service Model

Use `systemd --user`, not a system service:

- The daemon needs the user D-Bus session bus.
- Portal dialogs must appear in the user's graphical session.
- Wayland compositor permissions are user-session scoped.
- Running as root would not grant correct Wayland authority and would increase risk.
