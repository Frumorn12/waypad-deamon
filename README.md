# Waypad Daemon

Waypad-daemon is the Linux host service for Waypad: a secure Android-to-Linux remote-control system designed for Wayland desktops, especially Arch Linux, CachyOS, Hyprland, and wlroots-based sessions.

The daemon listens on the local network, pairs Android devices with explicit local approval, exposes host capabilities, and routes authenticated commands to Wayland portal input and safe system-control backends.

## Status

This is a shippable MVP foundation, not a finished remote-desktop suite. It implements secure pairing, encrypted sessions, trusted-device storage, discovery, diagnostics, systemd user integration, and a Wayland portal input backend. Full-screen streaming, cloud relay, iOS, and NAT traversal are intentionally out of scope.

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
- Hyprland `hyprctl` pointer-move fallback when RemoteDesktop is unavailable.
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
sudo pacman -S rust dbus xdg-desktop-portal xdg-desktop-portal-hyprland wireplumber playerctl brightnessctl wl-clipboard
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

## Install as a User Service

Waypad should run as a systemd user service because Wayland and portal access are tied to the logged-in graphical session.

```bash
cargo build --release
install -Dm755 target/release/waypad-daemon ~/.local/bin/waypad-daemon
install -Dm644 systemd/waypad-daemon.service ~/.config/systemd/user/waypad-daemon.service
systemctl --user daemon-reload
systemctl --user enable --now waypad-daemon
```

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
  "state_dir": "",
  "pairing_code_ttl_seconds": 300,
  "max_pair_attempts_per_minute": 5,
  "allow_suspend": false,
  "log_level": "info"
}
```

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
- LAN-only Android control.
- Pointer, clicks, scroll, keysyms, text, shortcuts, media, volume, brightness, clipboard set, lock.

Unsupported in MVP:

- X11 input injection.
- Root `/dev/uinput` bypass as the default backend.
- Internet exposure or cloud relay.
- Video streaming.
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

If the daemon reports `RemoteDesktop portal not available`, full input injection cannot work until the portal/compositor stack supports it. The Android app will still connect and show diagnostics, but pointer buttons, scroll, and keyboard commands will fail with explicit errors.

On Hyprland, Waypad can use a limited `hyprctl dispatch movecursor` fallback when RemoteDesktop is unavailable. This moves the cursor only. Clicks, scrolling, and keyboard input still require `org.freedesktop.portal.RemoteDesktop`.

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

- QR pairing payload for IP, port, code, and fingerprint.
- libei sender backend through RemoteDesktop `ConnectToEIS` where supported.
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
