//! MangoWC/dwl backend for Middlesox.
//!
//! This crate provides a `ProtocolAdapter` implementation for dwl-based
//! Wayland compositors (MangoWC, dwl) using native Wayland protocols.
//!
//! # Protocol Support
//!
//! Uses the `zdwl_ipc_manager` protocol for:
//! - Layout management
//! - Tag/workspace control
//! - Window state queries
//!
//! # Connection
//!
//! Uses `wayland-client` to connect to the Wayland display and
//! bind to the dwl IPC extension protocols.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use middlesox::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
use serde_json::Value;
use std::collections::HashSet;
use tokio::sync::mpsc;
use tracing::info;

/// MangoWC/dwl protocol adapter.
///
/// Connects to dwl-based compositors using native Wayland protocols.
pub struct MangoWcBackend {
    // TODO: Add wayland connection state
    // connection: wayland_client::Connection,
    // registry: wayland_client::protocol::wl_registry::WlRegistry,
}

impl MangoWcBackend {
    /// Create a new MangoWC backend.
    ///
    /// Connects to the default Wayland display.
    pub fn new() -> Result<Self> {
        // TODO: Connect to Wayland display
        // let connection = wayland_client::Connection::connect_to_env()?;
        Ok(Self {})
    }

    /// Create a backend with explicit display name.
    pub fn with_display(_name: &str) -> Result<Self> {
        // TODO: Connect to specific display
        Ok(Self {})
    }
}

impl Default for MangoWcBackend {
    fn default() -> Self {
        Self::new().expect("Failed to create MangoWC backend")
    }
}

#[async_trait]
impl ProtocolAdapter for MangoWcBackend {
    fn name(&self) -> &str {
        "mangowc"
    }

    fn manifest(&self) -> CapabilityManifest {
        // Capabilities based on zdwl_ipc_manager protocol
        CapabilityManifest::new()
            .add(Capability::read_write("layout").with_description("Window layout mode"))
            .add(Capability::read_write("tags").with_description("Active tag bitmask"))
            .add(Capability::read_only("title").with_description("Focused window title"))
            .add(Capability::read_only("appid").with_description("Focused window app ID"))
            .add(Capability::read_only("output").with_description("Current output name"))
    }

    async fn listen(
        &self,
        _event_tx: mpsc::Sender<RawEvent>,
        _subscriptions: HashSet<String>,
    ) -> Result<()> {
        // TODO: Implement Wayland event dispatch
        // Use wayland_client::Dispatch trait to handle protocol events
        // Convert zdwl_ipc events to RawEvent
        //
        // Only emit events that are in the subscriptions set
        info!("MangoWC backend: listen() not yet implemented");
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Value> {
        // TODO: Query state from Wayland globals
        Err(anyhow!("MangoWC get('{}') not yet implemented", key))
    }

    async fn set(&self, key: &str, _value: Value) -> Result<()> {
        // TODO: Send requests to zdwl_ipc_manager
        // Map keys to protocol requests:
        //   "layout" -> set_layout(layout_idx)
        //   "tags" -> set_tags(tag_mask)
        Err(anyhow!("MangoWC set('{}') not yet implemented", key))
    }
}

/// Create the MangoWC backend.
pub fn create_backend() -> Result<Box<dyn ProtocolAdapter>> {
    Ok(Box::new(MangoWcBackend::new()?))
}
