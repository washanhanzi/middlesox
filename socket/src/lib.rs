//! Socket-based protocol adapter for Middlesox.
//!
//! This crate provides a `ProtocolAdapter` implementation that communicates
//! over Unix sockets using a JSON Lines protocol. This enables external
//! bridge implementations in any language.
//!
//! # Architecture
//!
//! Uses two sockets for clean separation:
//! - **Command socket**: Request/response for get, set, caps
//! - **Event socket**: Push-based events from the bridge
//!
//! # Protocol (JSON Lines)
//!
//! ## Commands (Middlesox → Bridge)
//!
//! ```json
//! {"id": 1, "method": "get", "key": "workspace"}
//! {"id": 2, "method": "set", "key": "layout", "value": "grid"}
//! {"id": 3, "method": "caps"}
//! {"id": 4, "method": "subscribe", "events": ["workspace_change"]}
//! ```
//!
//! ## Responses (Bridge → Middlesox)
//!
//! ```json
//! {"id": 1, "result": {"id": 2, "name": "main"}}
//! {"id": 2, "result": null}
//! {"id": 3, "error": "unknown key"}
//! ```
//!
//! ## Events (Bridge → Middlesox, no id)
//!
//! ```json
//! {"event": "workspace_change", "prev": {"id": 1}, "curr": {"id": 2}}
//! ```

mod protocol;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use middlesox::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
use protocol::{Request, Response};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Socket-based protocol adapter.
///
/// Connects to external bridge processes via Unix sockets.
/// The bridge implements the actual WM/compositor communication.
pub struct SocketAdapter {
    /// Socket for commands (get, set, caps)
    cmd_socket: PathBuf,
    /// Socket for events (push from bridge)
    event_socket: PathBuf,
    /// Request ID counter
    next_id: AtomicU64,
    /// Cached capabilities (fetched once)
    cached_caps: tokio::sync::OnceCell<CapabilityManifest>,
}

impl SocketAdapter {
    /// Create a new socket adapter.
    ///
    /// # Arguments
    /// * `cmd_socket` - Path to the command socket (request/response)
    /// * `event_socket` - Path to the event socket (push events)
    pub fn new(cmd_socket: impl Into<PathBuf>, event_socket: impl Into<PathBuf>) -> Self {
        Self {
            cmd_socket: cmd_socket.into(),
            event_socket: event_socket.into(),
            next_id: AtomicU64::new(1),
            cached_caps: tokio::sync::OnceCell::new(),
        }
    }

    /// Create adapter from a base socket path.
    ///
    /// Derives command and event socket paths:
    /// - `{base}.sock` for commands
    /// - `{base}-events.sock` for events
    pub fn from_base_path(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        let cmd = base.with_extension("sock");
        let event = PathBuf::from(format!("{}-events.sock", base.display()));
        Self::new(cmd, event)
    }

    /// Send a request and wait for response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let request = Request {
            id,
            method: method.to_string(),
            params,
        };

        debug!("Sending request: {:?}", request);

        // Connect to command socket
        let mut stream = UnixStream::connect(&self.cmd_socket)
            .await
            .with_context(|| format!("Failed to connect to {}", self.cmd_socket.display()))?;

        // Send request
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;

        // Read response
        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await?;

        let response: Response = serde_json::from_str(&response_line)
            .with_context(|| format!("Invalid response: {}", response_line.trim()))?;

        debug!("Received response: {:?}", response);

        // Verify ID matches
        if response.id != id {
            return Err(anyhow!(
                "Response ID mismatch: expected {}, got {}",
                id,
                response.id
            ));
        }

        // Check for error
        if let Some(err) = response.error {
            return Err(anyhow!("Bridge error: {}", err));
        }

        Ok(response.result.unwrap_or(Value::Null))
    }

    /// Fetch capabilities from the bridge.
    async fn fetch_capabilities(&self) -> Result<CapabilityManifest> {
        let result = self.request("caps", Value::Null).await?;

        let caps: Vec<CapabilityInfo> =
            serde_json::from_value(result).context("Invalid capabilities response")?;

        let mut manifest = CapabilityManifest::new();
        for cap in caps {
            // Map access string to capability constructor
            // "rw" or "read_write" -> read_write, everything else -> read_only
            let capability = match cap.access.as_str() {
                "rw" | "read_write" | "readwrite" => Capability::read_write(&cap.name),
                _ => Capability::read_only(&cap.name),
            };

            let capability = if let Some(desc) = cap.description {
                capability.with_description(desc)
            } else {
                capability
            };

            manifest = manifest.add(capability);
        }

        Ok(manifest)
    }
}

/// Capability info from bridge.
#[derive(Debug, serde::Deserialize)]
struct CapabilityInfo {
    name: String,
    access: String,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl ProtocolAdapter for SocketAdapter {
    fn name(&self) -> &str {
        "socket"
    }

    fn manifest(&self) -> CapabilityManifest {
        // Return cached caps or empty manifest
        // (actual fetch happens async, so we cache on first get/set)
        self.cached_caps
            .get()
            .cloned()
            .unwrap_or_else(CapabilityManifest::new)
    }

    async fn listen(
        &self,
        event_tx: mpsc::Sender<RawEvent>,
        subscriptions: HashSet<String>,
    ) -> Result<()> {
        // Connect to event socket
        let stream = UnixStream::connect(&self.event_socket)
            .await
            .with_context(|| format!("Failed to connect to {}", self.event_socket.display()))?;

        debug!("Connected to event socket: {}", self.event_socket.display());

        // Send subscribe request
        let sub_request = Request {
            id: 0,
            method: "subscribe".to_string(),
            params: serde_json::json!({
                "events": subscriptions.iter().collect::<Vec<_>>()
            }),
        };

        let (reader, mut writer) = stream.into_split();

        let mut line = serde_json::to_string(&sub_request)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;

        debug!("Subscribed to events: {:?}", subscriptions);

        // Read events in a loop
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                // EOF - socket closed
                warn!("Event socket closed");
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Parse event
            match serde_json::from_str::<protocol::Event>(trimmed) {
                Ok(event) => {
                    debug!("Received event: {:?}", event);

                    let raw_event = RawEvent::new(event.event)
                        .with_prev_state(event.prev.map(json_object_to_hashmap).unwrap_or_default())
                        .with_curr_state(
                            event.curr.map(json_object_to_hashmap).unwrap_or_default(),
                        );

                    if event_tx.send(raw_event).await.is_err() {
                        // Receiver dropped
                        debug!("Event channel closed");
                        break;
                    }
                }
                Err(e) => {
                    // Might be a response to subscribe, skip it
                    if trimmed.contains("\"id\"") {
                        debug!("Skipping response: {}", trimmed);
                    } else {
                        error!("Failed to parse event: {} - {}", e, trimmed);
                    }
                }
            }
        }

        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Value> {
        // Ensure caps are cached
        let _ = self
            .cached_caps
            .get_or_try_init(|| self.fetch_capabilities())
            .await;

        self.request("get", serde_json::json!({"key": key})).await
    }

    async fn set(&self, key: &str, value: Value) -> Result<()> {
        // Ensure caps are cached
        let _ = self
            .cached_caps
            .get_or_try_init(|| self.fetch_capabilities())
            .await;

        self.request("set", serde_json::json!({"key": key, "value": value}))
            .await?;
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        // Nothing to clean up - connections are per-request
        Ok(())
    }
}

/// Convert a JSON object to a HashMap<String, Value>.
fn json_object_to_hashmap(value: Value) -> HashMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    }
}

/// Create a socket adapter from command and event socket paths.
pub fn create_backend(
    cmd_socket: impl Into<PathBuf>,
    event_socket: impl Into<PathBuf>,
) -> Box<dyn ProtocolAdapter> {
    Box::new(SocketAdapter::new(cmd_socket, event_socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_base_path() {
        let adapter = SocketAdapter::from_base_path("/run/user/1000/msx-hyprland");
        assert_eq!(
            adapter.cmd_socket,
            PathBuf::from("/run/user/1000/msx-hyprland.sock")
        );
        assert_eq!(
            adapter.event_socket,
            PathBuf::from("/run/user/1000/msx-hyprland-events.sock")
        );
    }
}
