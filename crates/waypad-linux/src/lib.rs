//! Linux/Wayland backends for the Waypad host.
//!
//! Implements the traits in `waypad_core::backend` on top of
//! xdg-desktop-portal, Hyprland IPC, PipeWire/GStreamer, uinput, and the
//! usual PulseAudio/PipeWire command-line helpers.
//!
//! The whole tree is gated on Linux so this crate can stay a workspace member
//! on every host: elsewhere it compiles to an empty library rather than
//! dragging zbus and libc into a build that has no use for them.

#![cfg(target_os = "linux")]

pub mod audio;
pub mod capability;
pub mod gamepad;
pub mod input;
pub mod platform;
pub mod screen;
pub mod system_control;
pub mod uinput;
