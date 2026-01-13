use crate::capability::CapabilityManifest;
use crate::event::RawEvent;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;

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
/// 2. Subscribing to and yielding WM events
/// 3. Querying WM state
/// 4. Executing commands against the WM
///
/// Only the adapter actor task calls these methods, so `&mut self` is
/// sufficient — no shared access or interior mutability needed.
#[async_trait]
pub trait ProtocolAdapter: Send {
    /// Returns the name of this adapter (e.g., "mangowc", "hyprland").
    fn name(&self) -> &str;

    /// Returns the capability manifest declaring what this adapter supports.
    ///
    /// The manifest is used by the security layer to validate script
    /// operations and by the core to understand available features.
    fn manifest(&self) -> CapabilityManifest;

    /// Subscribe to the given set of events.
    ///
    /// Called once after construction. The adapter should set up whatever
    /// internal state is needed to produce events matching `subscriptions`
    /// via [`next_event`](Self::next_event).
    async fn subscribe(&mut self, subscriptions: HashSet<String>) -> Result<()>;

    /// Yield the next event from the backend.
    ///
    /// Returns `Ok(Some(event))` when an event is available, or
    /// `Ok(None)` when the event source is exhausted (e.g., connection closed).
    ///
    /// # Cancel-safety
    ///
    /// This method is called inside `tokio::select!` in the adapter actor.
    /// Implementations **must** be cancel-safe. Strategies:
    /// - Use `tokio::time::Interval::tick()` (cancel-safe)
    /// - Use `mpsc::Receiver::recv()` with an internal reader task
    async fn next_event(&mut self) -> Result<Option<RawEvent>>;

    /// Query a value from the window manager.
    ///
    /// # Arguments
    /// * `key` - The capability key to query (e.g., "layout", "workspace")
    ///
    /// # Returns
    /// * `Ok(Value)` with the current value
    /// * `Err(_)` if the key is unknown or query fails
    async fn get(&mut self, key: &str) -> Result<Value>;

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
    async fn set(&mut self, key: &str, value: Value) -> Result<()>;

    /// Initialize the adapter.
    ///
    /// Called after construction but before the main event loop.
    /// Adapters that discover capabilities lazily (e.g., socket adapter
    /// fetching from a bridge) should pre-fetch and cache them here.
    async fn init(&mut self) -> Result<()> {
        Ok(())
    }

    /// Gracefully shutdown the adapter.
    ///
    /// Called when the controller is shutting down. Adapters should
    /// close connections and clean up resources.
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A boxed protocol adapter for dynamic dispatch.
pub type BoxedAdapter = Box<dyn ProtocolAdapter>;
