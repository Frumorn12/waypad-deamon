# Waypad Daemon

Waypad-daemon is the Linux host service for Waypad: a secure Android-to-Linux remote-control system designed for Wayland desktops, especially Arch Linux, CachyOS, Hyprland, and wlroots-based sessions.

The daemon listens on the local network, pairs Android devices with explicit local approval, exposes host capabilities, and routes authenticated commands to Wayland portal input and safe system-control backends.

## Status

This is a shippable MVP foundation, not a finished remote-desktop suite. It implements secure pairing, encrypted sessions, trusted-device storage, discovery, diagnostics, systemd user integration, Wayland portal input, external controller forwarding, QR invites, and a low-latency remote screen MVP. It supports LAN use and explicit direct-public endpoints for mobile-data tests. It does not bundle a cloud relay, automatic ICE/STUN/TURN traversal, or production WebRTC/H.264 streaming yet.

## Why Waypad Exists

Wayland deliberately prevents random processes from injecting input globally. That is a feature, not a bug. X11 tools such as `xdotool` are not the right model for Hyprland or modern Wayland desktops.

Waypad is built around the real Wayland path:

- Detect the graphical session.
- Detect xdg-desktop-portal.
- Detect `org.freedesktop.portal.RemoteDesktop`.
- Ask the local user for portal approval.
- Fail gracefully when the compositor or portal cannot support the action.

## Features

- Secure TCP control channel with P-256 ECDH, ECDSA host identity, HKDF, and AES-GCM frames.
- One-time local pairing code with rate limiting.
- Persistent trusted-device list with token hashes and revocation.
- UDP LAN discovery plus manual IP fallback.
- Capability endpoint for Wayland, portal, libei hints, volume, media, brightness, clipboard, lock, and suspend.
- Wayland RemoteDesktop portal backend for pointer, click, scroll, and keyboard keysyms when approved.
- Hyprland IPC fallback for pointer movement, mouse buttons, scroll, shortcuts, direct ASCII text, and clipboard-backed text for unsupported characters when RemoteDesktop is unavailable.
- External Android mouse and keyboard forwarding through the active pointer/keyboard backend.
- External Android controller/gamepad forwarding through an isolated Linux `uinput` virtual gamepad backend when `/dev/uinput` is available.
- Remote screen source discovery through XDG Desktop Portal ScreenCast and Hyprland monitor fallback.
- Token-negotiated direct TCP JPEG frame stream for Android screen viewing, with client-requested FPS, JPEG quality, and maximum frame dimension.
- Expiring `waypad://invite` QR payloads for terminal-driven pairing and direct-public bootstrap.
- Connectivity capability reporting for LAN direct, public direct, and unsupported relay/signaling/ICE/TURN cases.
- Absolute pointer command path for interaction with a displayed remote monitor.
- No X11-only injection hacks and no root-only default input path.
- `systemd --user` unit for correct user session and portal access.

## Repository Layout

```text
src/
  capability.rs       Wayland, portal, and helper detection
  config.rs           JSON config and defaults
  crypto.rs           Handshake and encrypted frame protocol
  discovery.rs        UDP discovery
  input.rs            Wayland portal input backend
  screen.rs           Screen sources, ScreenCast/PipeWire streaming, Hyprland grim fallback
  server.rs           Authenticated command server
  state.rs            Host identity and trusted devices
  system_control.rs   Volume/media/brightness/clipboard/system actions
docs/
  ARCHITECTURE.md
  PROTOCOL.md
  TROUBLESHOOTING.md
systemd/
  waypad-daemon.service
config/
  waypad-daemon.json
```

## Requirements

Minimum build/runtime:

- Rust 1.95 or newer.
- Linux user session.
- D-Bus session bus.
- Android Waypad app.

Recommended on Arch/CachyOS/Hyprland:

```bash
sudo pacman -S rust dbus xdg-desktop-portal xdg-desktop-portal-hyprland pipewire wireplumber gst-plugin-pipewire gst-plugins-good grim playerctl brightnessctl wl-clipboard
```

## Build

```bash
cargo build --release
cargo test
```

The daemon binary is:

```text
target/release/waypad-daemon
```

## Run in Foreground

```bash
cargo run -- doctor
cargo run -- serve
```

Create a local pairing code:

```bash
cargo run -- pair-code
```

Create a QR invite for the Android app:

```bash
cargo run -- invite --qr
```

## Install as a User Service

Waypad should run as a systemd user service because Wayland and portal access are tied to the logged-in graphical session.

```bash
cargo build --release
install -Dm755 target/release/waypad-daemon ~/.local/bin/waypad-daemon
install -Dm644 systemd/waypad-daemon.service ~/.config/systemd/user/waypad-daemon.service
systemctl --user daemon-reload
systemctl --user enable --now waypad-daemon
```

The unit is installed under `graphical-session.target` so it starts after the
user Wayland session has exported `WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`, and
compositor-specific variables to the systemd user manager.

Logs:

```bash
journalctl --user -u waypad-daemon -f
```

Stop or uninstall:

```bash
systemctl --user disable --now waypad-daemon
rm ~/.config/systemd/user/waypad-daemon.service
rm ~/.local/bin/waypad-daemon
systemctl --user daemon-reload
```

## Configuration

Default config path:

```text
~/.config/waypad-daemon/config.json
```

Write a sample:

```bash
waypad-daemon write-sample-config
```

Example:

```json
{
  "bind_address": "0.0.0.0",
  "control_port": 47771,
  "discovery_port": 47770,
  "require_private_lan": true,
  "allow_public_pairing": false,
  "state_dir": "",
  "pairing_code_ttl_seconds": 300,
  "max_pair_attempts_per_minute": 5,
  "allow_suspend": false,
  "log_level": "info"
}
```

- `require_private_lan`: When `true`, already-paired devices can reconnect from any network, but **new pairing from public IPs is blocked**. This is the safe default.
- `allow_public_pairing`: When `true`, the daemon accepts new pairing attempts from public IPs (mobile data) as long as the one-time pairing code is correct. Only enable this when your port is forwarded and protected by a firewall.

If `state_dir` is empty, the daemon uses:

```text
~/.local/state/waypad-daemon
```

Host identity, pairing code state, and trusted-device data are stored with private Unix permissions.

## Pairing Walkthrough

1. Start the daemon:

```bash
systemctl --user start waypad-daemon
```

2. Generate a local code:

```bash
waypad-daemon pair-code
```

3. Open the Android app, discover the host or enter its IP manually.

4. Enter the 6 digit code.

5. Compare the host fingerprint if pairing manually.

6. After pairing, tap "Approve portal" in the app and approve the Linux portal prompt if one appears.

Alternative QR flow:

```bash
waypad-daemon invite --qr
```

Scan the QR with the Android app's in-app scanner or paste the printed
`waypad://invite?...` payload. The invite embeds the host fingerprint, endpoint
hints, port, one-time pairing code, route type, expiry, and pairing policy.
By default the daemon chooses the LAN source address from the active IPv4 route;
override it with `--address` if the phone must use a different interface.

### Remote / mobile-data pairing

For a mobile-data/direct-public test, expose TCP `47771` through your firewall
or router and generate:

```bash
waypad-daemon invite --qr --remote-address your-public-hostname.example
```

The QR now contains **both** the public endpoint (`address` / `remote_address`)
and the LAN endpoint (`lan_address`). The Android app tries the public endpoint
first, then falls back to the LAN endpoint, so one QR works on mobile data
and on the same Wi-Fi.

Pairing policy depends on daemon config:

| `require_private_lan` | `allow_public_pairing` | Result for public IPs |
|---|---|---|
| `true` (default) | `false` (default) | **Pairing blocked.** Public clients can reconnect if already paired, but cannot pair for the first time. The QR policy will be `public-reconnect`. |
| `true` | `true` | **Pairing allowed.** Public clients can pair using the one-time 6-digit code. The QR policy will be `public-pairing`. |
| `false` | any | **All traffic allowed.** Public clients can pair and reconnect freely. |

To enable outside-LAN pairing safely, pick one of these options:

1. **Recommended** (keeps LAN-only restriction for reconnection):
   ```bash
   waypad-daemon write-sample-config
   # edit ~/.config/waypad-daemon/config.json
   # set "allow_public_pairing": true
   # restart the daemon
   ```
2. **Legacy** (allows all public traffic):
   ```bash
   # set "require_private_lan": false
   ```

Only do this if TCP `47771` is port-forwarded and protected by a firewall.
Pairing still requires the one-time 6-digit code, and all traffic is encrypted.
Full automatic NAT traversal still requires a future WebRTC/ICE/TURN backend.

## Device Management

```bash
waypad-daemon devices list
waypad-daemon devices revoke <device-id>
waypad-daemon rotate-host-key
```

Rotating the host key intentionally breaks existing Android trust pins. Re-pair afterward.

## Supported and Unsupported Scenarios

Supported:

- Linux host on Wayland.
- Hyprland/wlroots environments with working xdg-desktop-portal RemoteDesktop support.
- LAN Android control via discovery or QR invite.
- **Remote (mobile data) QR invites with explicit config:** When `allow_public_pairing=true` or `require_private_lan=false`, new devices can pair over the internet through a port-forwarded TCP `47771`. When `require_private_lan=true` (default), already-paired devices can reconnect from any network, but new pairing from public IPs is blocked.
- Pointer, clicks, scroll, keysyms, text, shortcuts, media, volume, brightness, clipboard set, lock.
- External mouse and keyboard devices connected to the Android phone when the Android app is in Pad or Screen mode.
- External Android controllers/gamepads through a Linux `uinput` virtual gamepad when `/dev/uinput` is available to the daemon user.
- Remote screen viewing through ScreenCast/PipeWire where portal streaming works.
- Hyprland monitor viewing through an isolated `grim` fallback when portal streaming dependencies are incomplete.

Unsupported in MVP:

- X11 input injection.
- Root `/dev/uinput` bypass as the default backend.
- Cloud relay, TURN fallback, and automatic ICE/STUN NAT traversal.
- End-to-end encrypted media stream separate from the encrypted control channel.
- Controller forwarding when the host does not expose writable `/dev/uinput` to the daemon user.
- WebRTC/H.264 transport and congestion-controlled adaptive bitrate.
- iOS client.

## Troubleshooting

Run:

```bash
waypad-daemon doctor
```

Common Hyprland fix:

```bash
sudo pacman -S xdg-desktop-portal xdg-desktop-portal-hyprland
systemctl --user restart xdg-desktop-portal xdg-desktop-portal-hyprland
waypad-daemon doctor
```

If the daemon reports `RemoteDesktop portal not available`, the portal-safe input path cannot work until the portal/compositor stack supports it. On Hyprland, Waypad falls back to the compositor IPC socket for practical local-session input.

On Hyprland, Waypad can expose the `hyprland-ipc` backend. It moves the cursor through Hyprland IPC, sends mouse button hold/release with `sendkeystate`, maps scroll to compositor mouse wheel shortcuts, and injects normal ASCII text as key events. Unsupported characters fall back to writing the requested text to `wl-copy` and sending `CTRL+V` to the active window. This keeps the daemon session-scoped and avoids root/uinput hacks, but the fallback paste path temporarily replaces the Wayland clipboard.

Controller forwarding is different from pointer/keyboard forwarding: Wayland portals and Hyprland IPC do not expose a generic gamepad injection API, so the daemon creates a normal Linux virtual gamepad with `uinput`. The daemon still runs as the user session; it just needs permission to open `/dev/uinput`. On systems where that node is unavailable or restricted, `waypad-daemon doctor` reports `external_input.controller = false` with the exact reason.

For remote screen mode, check the `capture` section in `waypad-daemon doctor`. Standard Wayland capture uses XDG Desktop Portal ScreenCast plus PipeWire and GStreamer. Hyprland systems can also expose monitor sources through the `hyprland-grim` fallback. If capture works but input fails, use screen viewing read-only or switch to Pad mode until RemoteDesktop or Hyprland IPC input is available.

For mobile-data/direct-public tests, check the `connectivity` section in
`waypad-daemon doctor`. Current builds report `lan_direct = true` and expose
direct-public invites when `require_private_lan = false`; `relay`, `stun`, and
`turn` intentionally remain false until a full WebRTC/relay stack is added.

More details are in `docs/TROUBLESHOOTING.md`.

## Protocol

The custom protocol is documented in `docs/PROTOCOL.md`. In short:

- UDP discovery advertises host name, port, fingerprint, and coarse capabilities.
- TCP handshake signs ephemeral ECDH parameters with the daemon host identity.
- AES-GCM encrypted frames carry pairing, authentication, and commands.
- Commands require authentication and monotonic sequence numbers.

## Development

```bash
cargo fmt
cargo test
cargo run -- doctor
RUST_LOG=debug cargo run -- serve
```

## Roadmap

- libei sender backend through RemoteDesktop `ConnectToEIS` where supported.
- WebRTC/H.264 media transport with ICE/STUN/TURN to replace the MVP JPEG frame stream for robust outside-LAN use.
- More detailed monitor/compositor diagnostics.
- Signed release packages.
- Broader integration tests with a fake protocol client.

## References

- XDG Desktop Portal RemoteDesktop: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html
- XDG Desktop Portal overview: https://flatpak.github.io/xdg-desktop-portal/
- libei documentation: https://libinput.pages.freedesktop.org/libei/
- Hyprland xdg-desktop-portal-hyprland notes: https://wiki.hypr.land/Hypr-Ecosystem/xdg-desktop-portal-hyprland/

## License

No open-source license has been selected yet. See `LICENSE`.
