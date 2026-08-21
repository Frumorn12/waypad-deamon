//! The platform-neutral half of a Waypad host.
//!
//! Contains the wire protocol, the encrypted control channel, pairing and
//! trusted-device storage, LAN discovery, and the screen/audio stream framing —
//! everything that must behave identically whether the desktop being controlled
//! runs Wayland or Windows.
//!
//! What it deliberately does not contain is any way to actually move a pointer
//! or capture a screen. That arrives through [`backend::PlatformHost`], which a
//! platform crate implements and the daemon binary selects at compile time.

pub mod audio;
pub mod backend;
pub mod capability;
pub mod config;
pub mod crypto;
pub mod discovery;
pub mod protocol;
pub mod server;
pub mod signal;
pub mod state;
pub mod stream;
