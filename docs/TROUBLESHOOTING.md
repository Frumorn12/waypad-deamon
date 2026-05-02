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

## "Remote input unavailable: RemoteDesktop portal not available"

Check:

```bash
busctl --user tree org.freedesktop.portal.Desktop
pacman -Qs xdg-desktop-portal
systemctl --user status xdg-desktop-portal xdg-desktop-portal-hyprland
```

Hyprland users should ensure `xdg-desktop-portal-hyprland` is installed and not masked.

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
