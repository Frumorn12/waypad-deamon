# Contributing

Waypad-daemon is a Wayland-first Linux daemon. Contributions should preserve the security model and avoid X11-only assumptions.

## Development Setup

```bash
cargo fmt
cargo test
cargo run -- doctor
```

For Hyprland testing on Arch or CachyOS:

```bash
sudo pacman -S xdg-desktop-portal xdg-desktop-portal-hyprland wireplumber playerctl brightnessctl wl-clipboard
systemctl --user restart xdg-desktop-portal xdg-desktop-portal-hyprland
cargo run -- doctor
```

## Rules for Changes

- Do not add unauthenticated control commands.
- Do not add root-only input injection or `/dev/uinput` hacks as default behavior.
- Keep Wayland portal, compositor-specific, and fallback paths isolated behind capability checks.
- Document any new command in `docs/PROTOCOL.md`.
- Add tests for protocol parsing, authorization, capability logic, or state handling when those areas change.

## Commit Hygiene

Keep daemon and Android changes in their separate repositories. This project intentionally uses two repos.
