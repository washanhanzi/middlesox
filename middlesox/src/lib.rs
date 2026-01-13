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
//! - **Adapter Handle**: `AdapterHandle` provides channel-based access to
//!   the adapter actor task (owns the adapter, no shared access needed).
//! - **Capability System**: Backends declare what they support via `Capability`
//!   and `CapabilityManifest`.
//! - **Event Pipeline**: `RawEvent`s flow from backends through the state
//!   store to trigger rule-matched scripts.
//! - **Scripting**: Rhai scripts can query and modify WM state with security
//!   enforcement.

mod adapter;
pub mod adapter_handle;
mod capability;
mod event;

pub mod config;
pub mod control;
pub mod controller;
pub mod engine;

// Core protocol types
pub use adapter::{BoxedAdapter, ProtocolAdapter};
pub use adapter_handle::AdapterHandle;
pub use capability::{AccessMode, Capability, CapabilityManifest};
pub use event::RawEvent;
