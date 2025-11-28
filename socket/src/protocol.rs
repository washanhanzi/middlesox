//! JSON Lines protocol types for socket communication.
//!
//! This module defines the wire format for communication between
//! Middlesox and external bridge processes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request from Middlesox to the bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Unique request ID for matching responses.
    pub id: u64,
    /// Method name: "get", "set", "caps", "subscribe"
    pub method: String,
    /// Method-specific parameters.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

/// A response from the bridge to Middlesox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Request ID this response is for.
    pub id: u64,
    /// Result value (present on success).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error message (present on failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An event pushed from the bridge to Middlesox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Event name (e.g., "workspace_change", "focus").
    pub event: String,
    /// Previous state (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<Value>,
    /// Current state (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curr: Option<Value>,
}

impl Request {
    /// Create a "get" request.
    pub fn get(id: u64, key: &str) -> Self {
        Self {
            id,
            method: "get".to_string(),
            params: serde_json::json!({"key": key}),
        }
    }

    /// Create a "set" request.
    pub fn set(id: u64, key: &str, value: Value) -> Self {
        Self {
            id,
            method: "set".to_string(),
            params: serde_json::json!({"key": key, "value": value}),
        }
    }

    /// Create a "caps" request.
    pub fn caps(id: u64) -> Self {
        Self {
            id,
            method: "caps".to_string(),
            params: Value::Null,
        }
    }

    /// Create a "subscribe" request.
    pub fn subscribe(id: u64, events: &[&str]) -> Self {
        Self {
            id,
            method: "subscribe".to_string(),
            params: serde_json::json!({"events": events}),
        }
    }
}

impl Response {
    /// Create a success response.
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(message.into()),
        }
    }
}

impl Event {
    /// Create a new event.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            event: name.into(),
            prev: None,
            curr: None,
        }
    }

    /// Set previous state.
    pub fn with_prev(mut self, prev: Value) -> Self {
        self.prev = Some(prev);
        self
    }

    /// Set current state.
    pub fn with_curr(mut self, curr: Value) -> Self {
        self.curr = Some(curr);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = Request::get(1, "workspace");
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"get\""));
        assert!(json.contains("\"key\":\"workspace\""));
    }

    #[test]
    fn test_response_success() {
        let resp = Response::success(1, serde_json::json!({"id": 2}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_response_error() {
        let resp = Response::error(1, "not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\":\"not found\""));
    }

    #[test]
    fn test_event_serialization() {
        let event = Event::new("workspace_change")
            .with_prev(serde_json::json!({"id": 1}))
            .with_curr(serde_json::json!({"id": 2}));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"workspace_change\""));
        assert!(json.contains("\"prev\""));
        assert!(json.contains("\"curr\""));
    }

    #[test]
    fn test_event_deserialization() {
        let json = r#"{"event":"focus","curr":{"title":"Firefox"}}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        assert_eq!(event.event, "focus");
        assert!(event.prev.is_none());
        assert!(event.curr.is_some());
    }
}
