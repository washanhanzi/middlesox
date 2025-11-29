//! Middlesox - Universal Window Manager Controller
//!
//! A scriptable event-driven controller for window managers and compositors.
//! Works with any WM/compositor that exposes an IPC mechanism.
//!
//! # Supported Platforms
//!
//! - **Wayland**: Hyprland, dwl/MangoWC, Sway, River
//! - **X11**: i3, bspwm, awesome, xmonad
//! - **macOS**: yabai, Amethyst, Aerospace
//! - **Windows**: komorebi, GlazeWM
//!
//! # Architecture
//!
//! - **Protocol Layer**: The `ProtocolAdapter` trait defines the interface
//!   that WM/compositor backends must implement.
//! - **Capability System**: Backends declare what they support via `Capability`
//!   and `CapabilityManifest`.
//! - **Event Pipeline**: `RawEvent`s flow from backends through the state
//!   store to trigger rule-matched scripts.
//! - **Scripting**: Rhai scripts can query and modify WM state with security
//!   enforcement.

mod adapter;
mod capability;
mod event;

#[cfg(feature = "mock")]
mod mock;

pub mod config;
pub mod control;
pub mod engine;

// Core protocol types
pub use adapter::{BoxedAdapter, ProtocolAdapter};
pub use capability::{AccessMode, Capability, CapabilityManifest};
pub use event::RawEvent;

// Mock backend (dev/testing only)
#[cfg(feature = "mock")]
pub use mock::MockBackend;

/// Create a built-in backend by name.
///
/// Available backends depend on enabled features:
/// - "mock": Simulated backend for testing (requires `mock` feature)
///
/// For real WM backends, use the separate crates:
/// - `middlesox-hyprland` for Hyprland
/// - `middlesox-mangowc` for MangoWC/dwl
pub fn create_backend(name: &str) -> Option<BoxedAdapter> {
    match name {
        #[cfg(feature = "mock")]
        "mock" => Some(Box::new(MockBackend::new())),
        _ => None,
    }
}

/// List all available built-in backend names.
#[allow(unused_mut)]
pub fn available_backends() -> Vec<&'static str> {
    let mut backends = Vec::new();
    #[cfg(feature = "mock")]
    backends.push("mock");
    backends
}
