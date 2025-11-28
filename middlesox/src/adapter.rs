use crate::capability::CapabilityManifest;
use crate::event::RawEvent;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use tokio::sync::mpsc;

/// The core trait that all window manager backends must implement.
///
/// This trait defines the contract between the core controller and
/// WM/compositor-specific implementations. Platform agnostic - works with:
///
/// - **Wayland**: Hyprland, Sway, dwl, River
/// - **X11**: i3, bspwm, awesome, xmonad
/// - **macOS**: yabai, Amethyst, Aerospace
/// - **Windows**: komorebi, GlazeWM
///
/// Each adapter is responsible for:
/// 1. Declaring its capabilities (what it can read/write)
/// 2. Listening for WM events and forwarding them
/// 3. Querying WM state
/// 4. Executing commands against the WM
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    /// Returns the name of this adapter (e.g., "mangowc", "hyprland").
    fn name(&self) -> &str;

    /// Returns the capability manifest declaring what this adapter supports.
    ///
    /// The manifest is used by the security layer to validate script
    /// operations and by the core to understand available features.
    fn manifest(&self) -> CapabilityManifest;

    /// Start the event loop, pushing subscribed events to the provided channel.
    ///
    /// This method should run indefinitely, listening for WM/compositor
    /// events and converting them to `RawEvent` instances. Only events
    /// whose names are in `subscriptions` should be emitted.
    ///
    /// # Arguments
    /// * `event_tx` - Channel sender for emitting events to the core
    /// * `subscriptions` - Set of event names to subscribe to. Only emit
    ///   events whose names are in this set. If empty, emit nothing.
    ///
    /// # Returns
    /// * `Ok(())` if the loop exits gracefully (e.g., shutdown signal)
    /// * `Err(_)` if there's a connection error or fatal failure
    async fn listen(
        &self,
        event_tx: mpsc::Sender<RawEvent>,
        subscriptions: HashSet<String>,
    ) -> Result<()>;

    /// Query a value from the window manager.
    ///
    /// # Arguments
    /// * `key` - The capability key to query (e.g., "layout", "workspace")
    ///
    /// # Returns
    /// * `Ok(Value)` with the current value
    /// * `Err(_)` if the key is unknown or query fails
    async fn get(&self, key: &str) -> Result<Value>;

    /// Set a value in the window manager.
    ///
    /// # Arguments
    /// * `key` - The capability key to set (e.g., "layout")
    /// * `value` - The new value to set
    ///
    /// # Returns
    /// * `Ok(())` if the command was sent successfully
    /// * `Err(_)` if the key is unknown, read-only, or command fails
    ///
    /// # Security Note
    /// The core's security layer validates write permissions before
    /// calling this method. Adapters should still validate inputs.
    async fn set(&self, key: &str, value: Value) -> Result<()>;

    /// Gracefully shutdown the adapter.
    ///
    /// Called when the controller is shutting down. Adapters should
    /// close connections and clean up resources.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// A boxed protocol adapter for dynamic dispatch.
pub type BoxedAdapter = Box<dyn ProtocolAdapter>;
