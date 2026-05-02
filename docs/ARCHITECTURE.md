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
| `capability` | Session, portal, libei, and system helper detection. |
| `input` | Wayland RemoteDesktop portal backend and unsupported fallback. |
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

## Security Boundaries

The daemon binds to LAN by default but still treats the LAN as hostile:

- Unknown clients only receive a signed handshake.
- Pairing requires a local one-time code.
- Commands require authentication.
- Device tokens can be revoked.
- Host key rotation invalidates existing trust.
- Public internet source addresses are rejected when `require_private_lan` is enabled.

## Service Model

Use `systemd --user`, not a system service:

- The daemon needs the user D-Bus session bus.
- Portal dialogs must appear in the user's graphical session.
- Wayland compositor permissions are user-session scoped.
- Running as root would not grant correct Wayland authority and would increase risk.
