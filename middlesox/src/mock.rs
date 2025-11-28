//! Mock backend for testing the core logic without a real window manager.
//!
//! This backend simulates workspace changes and layout toggles,
//! allowing development and testing of the event pipeline and scripting.

use crate::adapter::ProtocolAdapter;
use crate::capability::{Capability, CapabilityManifest};
use crate::event::RawEvent;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};
use tracing::{debug, info};

/// A mock protocol adapter that simulates window manager events.
///
/// Useful for testing the event pipeline and scripting engine
/// without needing a running window manager or compositor.
pub struct MockBackend {
    /// Current workspace (1-10)
    workspace: AtomicI64,
    /// Current layout ("master", "grid", "float")
    layout: RwLock<String>,
    /// Whether the backend is running
    running: AtomicBool,
    /// Event generation interval in milliseconds
    event_interval_ms: u64,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            workspace: AtomicI64::new(1),
            layout: RwLock::new("master".into()),
            running: AtomicBool::new(false),
            event_interval_ms: 2000,
        }
    }

    /// Set the event generation interval.
    pub fn with_interval(mut self, ms: u64) -> Self {
        self.event_interval_ms = ms;
        self
    }

    /// Stop the event loop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProtocolAdapter for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    fn manifest(&self) -> CapabilityManifest {
        CapabilityManifest::new()
            .add(
                Capability::read_write("workspace")
                    .with_description("Current workspace number (1-10)"),
            )
            .add(
                Capability::read_write("layout")
                    .with_description("Window layout mode (master, grid, float)"),
            )
            .add(
                Capability::read_only("monitor")
                    .with_description("Current monitor name"),
            )
            .add(
                Capability::read_only("window_count")
                    .with_description("Number of windows on current workspace"),
            )
    }

    async fn listen(
        &self,
        event_tx: mpsc::Sender<RawEvent>,
        subscriptions: HashSet<String>,
    ) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        let mut tick = interval(Duration::from_millis(self.event_interval_ms));
        let mut cycle = 0u64;

        info!(
            "Mock backend started, subscribed to: {:?}",
            subscriptions.iter().collect::<Vec<_>>()
        );

        if subscriptions.is_empty() {
            info!("No subscriptions, mock backend idle");
            // Just wait for shutdown
            while self.running.load(Ordering::SeqCst) {
                tick.tick().await;
            }
            return Ok(());
        }

        while self.running.load(Ordering::SeqCst) {
            tick.tick().await;
            cycle += 1;

            // Generate events based on cycle, but only emit if subscribed
            let event = match cycle % 3 {
                0 if subscriptions.contains("workspace_change") => {
                    // Workspace change: prev -> curr transition
                    let prev_ws = self.workspace.load(Ordering::SeqCst);
                    let curr_ws = (prev_ws % 4) + 1; // Cycle through 1-4
                    self.workspace.store(curr_ws, Ordering::SeqCst);

                    debug!("Mock: workspace {} -> {}", prev_ws, curr_ws);

                    Some(
                        RawEvent::new("workspace_change")
                            .with_prev("id", prev_ws)
                            .with_curr("id", curr_ws)
                            .with_curr("monitor", "MOCK-1"),
                    )
                }
                1 if subscriptions.contains("window_update") => {
                    // Window count update (curr state only)
                    let count = (cycle % 5) as i64 + 1;
                    debug!("Mock: window_count = {}", count);

                    Some(
                        RawEvent::new("window_update")
                            .with_curr("window_count", count)
                            .with_curr("workspace", self.workspace.load(Ordering::SeqCst)),
                    )
                }
                2 if subscriptions.contains("layout_hint") => {
                    // Layout hint (curr state only)
                    let layout = self.layout.read().await.clone();
                    debug!("Mock: layout_hint = {}", layout);

                    Some(
                        RawEvent::new("layout_hint")
                            .with_curr("layout", layout)
                            .with_curr("workspace", self.workspace.load(Ordering::SeqCst)),
                    )
                }
                _ => None, // Not subscribed or no event this cycle
            };

            if let Some(event) = event {
                if event_tx.send(event).await.is_err() {
                    info!("Mock backend: event channel closed, stopping");
                    break;
                }
            }
        }

        info!("Mock backend stopped");
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Value> {
        match key {
            "workspace" => Ok(Value::from(self.workspace.load(Ordering::SeqCst))),
            "layout" => Ok(Value::from(self.layout.read().await.clone())),
            "monitor" => Ok(Value::from("MOCK-1")),
            "window_count" => Ok(Value::from(3)), // Simulated
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn set(&self, key: &str, value: Value) -> Result<()> {
        match key {
            "workspace" => {
                let ws = value
                    .as_i64()
                    .ok_or_else(|| anyhow!("workspace must be an integer"))?;
                if !(1..=10).contains(&ws) {
                    return Err(anyhow!("workspace must be 1-10"));
                }
                info!("Mock: SET workspace = {}", ws);
                self.workspace.store(ws, Ordering::SeqCst);
                Ok(())
            }
            "layout" => {
                let layout = value
                    .as_str()
                    .ok_or_else(|| anyhow!("layout must be a string"))?;
                let valid = ["master", "grid", "float"];
                if !valid.contains(&layout) {
                    return Err(anyhow!("layout must be one of: {:?}", valid));
                }
                info!("Mock: SET layout = {}", layout);
                *self.layout.write().await = layout.to_string();
                Ok(())
            }
            "monitor" | "window_count" => {
                Err(anyhow!("'{}' is read-only", key))
            }
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        self.stop();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_get_set() {
        let backend = MockBackend::new();

        // Test get
        let ws = backend.get("workspace").await.unwrap();
        assert_eq!(ws, Value::from(1));

        // Test set
        backend.set("workspace", Value::from(5)).await.unwrap();
        let ws = backend.get("workspace").await.unwrap();
        assert_eq!(ws, Value::from(5));

        // Test layout
        backend.set("layout", Value::from("grid")).await.unwrap();
        let layout = backend.get("layout").await.unwrap();
        assert_eq!(layout, Value::from("grid"));
    }

    #[tokio::test]
    async fn test_mock_readonly() {
        let backend = MockBackend::new();

        // Should fail on read-only
        let result = backend.set("monitor", Value::from("test")).await;
        assert!(result.is_err());
    }
}
