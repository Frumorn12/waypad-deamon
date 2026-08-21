//! Windows backends for the Waypad host.
//!
//! Implements the traits in `waypad_core::backend` with SendInput, DXGI
//! Desktop Duplication, Media Foundation, and WASAPI.
//!
//! Gated on Windows so the crate can stay a workspace member everywhere:
//! elsewhere it compiles to an empty library instead of dragging the Win32
//! bindings into a build that has no use for them.

#![cfg(windows)]

pub mod input;
