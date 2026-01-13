//! Mock backend for testing the core logic without a real window manager.
//!
//! This backend simulates workspace changes and layout toggles,
//! allowing development and testing of the event pipeline and scripting.

use middlesox::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use tokio::time::{interval, Duration, Interval};
use tracing::{debug, info};

/// A mock protocol adapter that simulates window manager events.
///
/// Useful for testing the event pipeline and scripting engine
/// without needing a running window manager or compositor.
pub struct MockBackend {
    /// Current workspace (1-10)
    workspace: i64,
    /// Current layout ("master", "grid", "float")
    layout: String,
    /// Event generation interval in milliseconds
    event_interval_ms: u64,
    /// Subscribed event names
    subscriptions: HashSet<String>,
    /// Tick interval (initialized on subscribe)
    tick: Option<Interval>,
    /// Event cycle counter
    cycle: u64,
    /// Whether the backend has been shut down
    shutdown: bool,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            workspace: 1,
            layout: "master".into(),
            event_interval_ms: 2000,
            subscriptions: HashSet::new(),
            tick: None,
            cycle: 0,
            shutdown: false,
        }
    }

    /// Set the event generation interval.
    pub fn with_interval(mut self, ms: u64) -> Self {
        self.event_interval_ms = ms;
        self
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

    async fn subscribe(&mut self, subscriptions: HashSet<String>) -> Result<()> {
        info!(
            "Mock backend subscribed to: {:?}",
            subscriptions.iter().collect::<Vec<_>>()
        );
        self.subscriptions = subscriptions;
        self.tick = Some(interval(Duration::from_millis(self.event_interval_ms)));
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<RawEvent>> {
        if self.shutdown {
            return Ok(None);
        }

        let tick = self.tick.as_mut().expect("subscribe() must be called before next_event()");

        if self.subscriptions.is_empty() {
            // No subscriptions — just wait until shutdown
            std::future::pending::<()>().await;
            return Ok(None);
        }

        loop {
            tick.tick().await;
            self.cycle += 1;

            let event = match self.cycle % 3 {
                0 if self.subscriptions.contains("workspace_change") => {
                    let prev_ws = self.workspace;
                    let curr_ws = (prev_ws % 4) + 1;
                    self.workspace = curr_ws;

                    debug!("Mock: workspace {} -> {}", prev_ws, curr_ws);

                    Some(
                        RawEvent::new("workspace_change")
                            .with_prev("id", prev_ws)
                            .with_curr("id", curr_ws)
                            .with_curr("monitor", "MOCK-1"),
                    )
                }
                1 if self.subscriptions.contains("window_update") => {
                    let count = (self.cycle % 5) as i64 + 1;
                    debug!("Mock: window_count = {}", count);

                    Some(
                        RawEvent::new("window_update")
                            .with_curr("window_count", count)
                            .with_curr("workspace", self.workspace),
                    )
                }
                2 if self.subscriptions.contains("layout_hint") => {
                    let layout = self.layout.clone();
                    debug!("Mock: layout_hint = {}", layout);

                    Some(
                        RawEvent::new("layout_hint")
                            .with_curr("layout", layout)
                            .with_curr("workspace", self.workspace),
                    )
                }
                _ => None,
            };

            if let Some(event) = event {
                return Ok(Some(event));
            }
        }
    }

    async fn get(&mut self, key: &str) -> Result<Value> {
        match key {
            "workspace" => Ok(Value::from(self.workspace)),
            "layout" => Ok(Value::from(self.layout.clone())),
            "monitor" => Ok(Value::from("MOCK-1")),
            "window_count" => Ok(Value::from(3)), // Simulated
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn set(&mut self, key: &str, value: Value) -> Result<()> {
        match key {
            "workspace" => {
                let ws = value
                    .as_i64()
                    .ok_or_else(|| anyhow!("workspace must be an integer"))?;
                if !(1..=10).contains(&ws) {
                    return Err(anyhow!("workspace must be 1-10"));
                }
                info!("Mock: SET workspace = {}", ws);
                self.workspace = ws;
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
                self.layout = layout.to_string();
                Ok(())
            }
            "monitor" | "window_count" => {
                Err(anyhow!("'{}' is read-only", key))
            }
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.shutdown = true;
        Ok(())
    }
}

/// Create a mock backend instance.
pub fn create_backend() -> Box<dyn ProtocolAdapter> {
    Box::new(MockBackend::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_get_set() {
        let mut backend = MockBackend::new();

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
        let mut backend = MockBackend::new();

        // Should fail on read-only
        let result = backend.set("monitor", Value::from("test")).await;
        assert!(result.is_err());
    }
}
