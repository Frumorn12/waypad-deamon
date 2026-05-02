# Troubleshooting

## Run Diagnostics

```bash
waypad-daemon doctor
```

Watch logs:

```bash
journalctl --user -u waypad-daemon -f
```

## Hyprland on Arch or CachyOS

Install portal and useful helpers:

```bash
sudo pacman -S xdg-desktop-portal xdg-desktop-portal-hyprland wireplumber playerctl brightnessctl wl-clipboard
systemctl --user restart xdg-desktop-portal xdg-desktop-portal-hyprland
waypad-daemon doctor
```

If `RemoteDesktop` is unavailable, the daemon cannot use the portal input path. This is a compositor/portal capability issue, not an Android networking issue.

On Hyprland, Waypad may still expose the `hyprland-ipc` fallback. That backend talks to the user-session Hyprland IPC socket directly, supports pointer motion, mouse button hold/release, wheel-style scrolling, shortcuts, and direct ASCII text events. Unsupported text falls back to `wl-copy` paste, which requires `wl-clipboard` and temporarily replaces the current Wayland clipboard.

For screen viewing on Hyprland, install PipeWire/GStreamer helpers if you want the standard portal stream path:

```bash
sudo pacman -S pipewire wireplumber gst-plugin-pipewire gst-plugins-good grim
systemctl --user restart pipewire wireplumber xdg-desktop-portal xdg-desktop-portal-hyprland
waypad-daemon doctor
```

If the portal stream path is incomplete but Hyprland and `grim` are available, Waypad exposes concrete monitor sources through the isolated `hyprland-grim` fallback.

## "Remote input unavailable: RemoteDesktop portal not available"

Check:

```bash
busctl --user tree org.freedesktop.portal.Desktop
pacman -Qs xdg-desktop-portal
systemctl --user status xdg-desktop-portal xdg-desktop-portal-hyprland
```

Hyprland users should ensure `xdg-desktop-portal-hyprland` is installed and not masked.

## "Screen capture unavailable: ScreenCast portal not available"

Check the ScreenCast portal:

```bash
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.ScreenCast --no-pager
systemctl --user status xdg-desktop-portal
```

On Hyprland, ensure `xdg-desktop-portal-hyprland` is installed and running. On GNOME/KDE, use the desktop's portal backend and update the portal packages if the interface is missing.

## "PipeWire capture could not be initialized"

Check PipeWire and GStreamer:

```bash
systemctl --user status pipewire wireplumber
gst-inspect-1.0 pipewiresrc
gst-inspect-1.0 jpegenc
```

If `pipewiresrc` is missing, install the PipeWire GStreamer plugin package for your distribution.

## Stream Starts But Input Fails

This is a normal partial-support case. Capture and control are separate capabilities. The app can show the screen while the daemon reports that RemoteDesktop input is blocked or unsupported.

For portal input, tap "Approve portal" in the app and approve pointer/keyboard control on the Linux host. For Hyprland fallback, confirm `waypad-daemon doctor` reports `input.backend = hyprland-ipc`.

## External Mouse Or Keyboard On Android Does Nothing

The Android app forwards external devices only while connected in Pad or Screen mode. On the host, check capabilities:

```bash
waypad-daemon doctor | grep -A8 external_input
journalctl --user -u waypad-daemon -f
```

`external_input.pointer` and `external_input.keyboard` follow the normal input backend. If they are false, fix RemoteDesktop portal approval or the Hyprland IPC fallback first. If Android logs show `external_input_unsupported`, the host is explicitly rejecting that class rather than dropping it silently.

## Controller Or Gamepad Forwarding Does Not Work

Android controller detection and protocol transport are implemented. The host-side injection path uses Linux `uinput`, so first check:

```bash
waypad-daemon doctor | grep -A8 external_input
ls -l /dev/uinput
journalctl --user -u waypad-daemon -f
```

If `external_input.controller = true`, open the remote screen in Waypad, keep the Android app focused/fullscreen, and press a controller button. The daemon should log that an Android controller attached to the virtual gamepad, and browser tests such as `hardwaretester.com/gamepad` should see `Waypad Android Virtual Gamepad` on the PC.

If `external_input.controller = false`, the reason usually says `/dev/uinput` is missing or not writable. Load the `uinput` kernel module and add a udev rule or group policy that allows the Waypad user to open `/dev/uinput`; do not run the whole daemon as root just for controller support. After changing permissions, restart `waypad-daemon`.

## Android Reports "Connection Closed" Or "Broken Pipe"

Watch daemon logs while pressing Start in the Android Screen tab:

```bash
journalctl --user -u waypad-daemon -f
```

Healthy current logs show:

```text
screen stream session pending client attach ... stream_port=47771
screen stream attach request received on control port
screen stream client attached ...
```

If logs show a random high `stream_port`, the Android app and daemon are from mismatched builds. Rebuild the daemon, install it, and restart the user service:

```bash
cargo build --release
install -Dm755 target/release/waypad-daemon ~/.local/bin/waypad-daemon
systemctl --user restart waypad-daemon
```

If Android still cannot connect to `47771`, confirm the daemon is listening and the phone can reach the host IP:

```bash
ss -ltnp | grep 47771
ip -4 addr
```

## QR Invite Shows 127.0.0.1 Or The Phone Cannot Connect

Current builds choose the LAN address from the active IPv4 route:

```bash
ip -4 route get 1.1.1.1
waypad-daemon invite --qr
```

The QR payload should contain the `src` address from that route, not
`127.0.0.1`. If the phone must use a different interface, pass it explicitly:

```bash
waypad-daemon invite --qr --address 192.168.0.184
```

For mobile-data testing, expose the daemon's TCP port intentionally and provide
the reachable public endpoint:

```bash
waypad-daemon invite --qr --remote-address your-public-hostname.example
```

This is direct TCP. The daemon does not provide a relay, STUN, TURN, or automatic
ICE traversal yet. If `require_private_lan` is true, public clients are rejected
even if they have a valid invite.

## 60 FPS Setting Does Not Seem To Apply

The Android app sends `max_fps`, `jpeg_quality`, `max_width`, and `max_height`
when starting a screen stream. The daemon logs the accepted values:

```bash
journalctl --user -u waypad-daemon -f | grep 'starting screen stream'
```

For Game Mode or Ultra Low Latency, expect `fps=60` and a smaller max dimension.
Actual delivered FPS still depends on compositor capture speed, PipeWire/GStreamer
availability, JPEG encode speed, Wi-Fi quality, and Android decode time. The
daemon drops stale frames rather than buffering them, so FPS may fall under load
to keep input latency lower.

## Input Works But Stream Fails

Check `capture` in `waypad-daemon doctor`. Input may use RemoteDesktop or Hyprland IPC even when ScreenCast/PipeWire is unavailable. Use the app's Pad mode as a fallback while fixing portal or PipeWire capture.

## "Input injection requires portal approval"

Open the Android app, connect, then tap "Approve portal". A local portal dialog should appear on the Linux host. Approve keyboard and pointer control.

## Pairing Fails

Create a fresh code:

```bash
waypad-daemon pair-code
```

Pairing codes expire after 5 minutes by default and are single use.

## Device Was Lost or Sold

Revoke it:

```bash
waypad-daemon devices list
waypad-daemon devices revoke <device-id>
```

## Host Fingerprint Changed

The Android app refuses to connect if the pinned host fingerprint changes. This can happen after:

- `waypad-daemon rotate-host-key`
- deleting the daemon state directory
- restoring from a different Linux user profile

Remove the trusted host on Android and pair again only if you intentionally changed the host key.
