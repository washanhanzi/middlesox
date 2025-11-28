//! Hyprland backend for Middlesox.
//!
//! This crate provides a `ProtocolAdapter` implementation for the
//! Hyprland Wayland compositor using its Unix socket IPC.
//!
//! # Connection
//!
//! Hyprland exposes two sockets:
//! - `.socket.sock` - For commands (hyprctl)
//! - `.socket2.sock` - For events (stream)
//!
//! Located at: `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`
//!
//! # Events
//!
//! Events are newline-delimited text in format: `event>>data`
//! Examples:
//! - `workspace>>2`
//! - `activewindow>>kitty,Terminal`
//! - `fullscreen>>1`

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use middlesox::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
use serde_json::Value;
use std::collections::HashSet;
use tokio::sync::mpsc;
use tracing::info;

/// Hyprland protocol adapter.
///
/// Connects to Hyprland's IPC sockets for event listening and command execution.
pub struct HyprlandBackend {
    // TODO: Add socket paths and connection state
}

impl HyprlandBackend {
    /// Create a new Hyprland backend.
    ///
    /// Automatically discovers the socket path from environment variables.
    pub fn new() -> Result<Self> {
        // TODO: Discover socket path from $XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/
        Ok(Self {})
    }

    /// Create a backend with explicit socket path.
    pub fn with_socket_path(_path: impl Into<std::path::PathBuf>) -> Self {
        Self {}
    }
}

impl Default for HyprlandBackend {
    fn default() -> Self {
        Self::new().expect("Failed to create Hyprland backend")
    }
}

#[async_trait]
impl ProtocolAdapter for HyprlandBackend {
    fn name(&self) -> &str {
        "hyprland"
    }

    fn manifest(&self) -> CapabilityManifest {
        CapabilityManifest::new()
            .add(Capability::read_write("workspace").with_description("Active workspace ID"))
            .add(Capability::read_only("activewindow").with_description("Currently focused window"))
            .add(Capability::read_only("monitors").with_description("Connected monitors"))
            .add(Capability::read_write("fullscreen").with_description("Fullscreen state"))
    }

    async fn listen(
        &self,
        _event_tx: mpsc::Sender<RawEvent>,
        _subscriptions: HashSet<String>,
    ) -> Result<()> {
        // TODO: Connect to .socket2.sock and parse event stream
        // Format: event>>data\n
        // Examples:
        //   workspace>>2
        //   activewindow>>kitty,Terminal
        //   fullscreen>>1
        //
        // Only emit events that are in the subscriptions set
        info!("Hyprland backend: listen() not yet implemented");
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Value> {
        // TODO: Execute hyprctl commands and parse JSON output
        // hyprctl -j activewindow
        // hyprctl -j monitors
        // hyprctl -j workspaces
        Err(anyhow!("Hyprland get('{}') not yet implemented", key))
    }

    async fn set(&self, key: &str, _value: Value) -> Result<()> {
        // TODO: Execute hyprctl dispatch commands
        // hyprctl dispatch workspace 2
        // hyprctl dispatch fullscreen 1
        Err(anyhow!("Hyprland set('{}') not yet implemented", key))
    }
}

/// Create the Hyprland backend.
pub fn create_backend() -> Result<Box<dyn ProtocolAdapter>> {
    Ok(Box::new(HyprlandBackend::new()?))
}
