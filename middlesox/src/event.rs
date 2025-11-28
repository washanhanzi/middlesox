use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// A raw event emitted by a protocol adapter.
///
/// Events represent state transitions with optional previous and current state.
/// This allows matching on transitions like "focus changed from window A to window B".
///
/// # Examples
///
/// ```ignore
/// // Focus change event
/// RawEvent::new("focus_change")
///     .with_prev("title", "Terminal")
///     .with_curr("title", "Browser")
///
/// // Workspace change event
/// RawEvent::new("workspace_change")
///     .with_prev("id", 1)
///     .with_curr("id", 2)
///     .with_curr("monitor", "HDMI-1")
///
/// // Window created (no previous state)
/// RawEvent::new("window_create")
///     .with_curr("title", "New Window")
///     .with_curr("appid", "kitty")
///
/// // Window closed (no current state)
/// RawEvent::new("window_close")
///     .with_prev("title", "Closed Window")
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    /// Event name (e.g., "focus_change", "workspace_change", "window_create")
    pub name: String,
    /// Previous state before the event (None if not applicable, e.g., window creation)
    pub prev: Option<HashMap<String, Value>>,
    /// Current state after the event (None if not applicable, e.g., window destruction)
    pub curr: Option<HashMap<String, Value>>,
    /// Timestamp when the event was received (Unix millis)
    pub timestamp: u64,
}

impl RawEvent {
    /// Create a new event with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prev: None,
            curr: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// Add a key-value pair to the previous state.
    pub fn with_prev(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.prev
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Add a key-value pair to the current state.
    pub fn with_curr(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.curr
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Set the entire previous state.
    pub fn with_prev_state(mut self, state: HashMap<String, Value>) -> Self {
        self.prev = Some(state);
        self
    }

    /// Set the entire current state.
    pub fn with_curr_state(mut self, state: HashMap<String, Value>) -> Self {
        self.curr = Some(state);
        self
    }

    /// Get a value from the previous state.
    pub fn get_prev(&self, key: &str) -> Option<&Value> {
        self.prev.as_ref()?.get(key)
    }

    /// Get a value from the current state.
    pub fn get_curr(&self, key: &str) -> Option<&Value> {
        self.curr.as_ref()?.get(key)
    }

    /// Get a string from the previous state.
    pub fn get_prev_str(&self, key: &str) -> Option<&str> {
        self.get_prev(key)?.as_str()
    }

    /// Get a string from the current state.
    pub fn get_curr_str(&self, key: &str) -> Option<&str> {
        self.get_curr(key)?.as_str()
    }

    /// Get an i64 from the previous state.
    pub fn get_prev_i64(&self, key: &str) -> Option<i64> {
        self.get_prev(key)?.as_i64()
    }

    /// Get an i64 from the current state.
    pub fn get_curr_i64(&self, key: &str) -> Option<i64> {
        self.get_curr(key)?.as_i64()
    }
}
