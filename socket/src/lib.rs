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
//! ## Commands (Middlesox -> Bridge)
//!
//! ```json
//! {"id": 1, "method": "get", "params": {"key": "workspace"}}
//! {"id": 2, "method": "set", "params": {"key": "layout", "value": "grid"}}
//! {"id": 3, "method": "caps"}
//! {"id": 4, "method": "subscribe", "params": {"events": ["workspace_change"]}}
//! ```
//!
//! ## Responses (Bridge -> Middlesox)
//!
//! ```json
//! {"id": 1, "result": {"id": 2, "name": "main"}}
//! {"id": 2, "result": null}
//! {"id": 3, "error": "unknown key"}
//! ```
//!
//! ## Events (Bridge -> Middlesox, no id)
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
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Timeout for command round-trips to the bridge process.
const BRIDGE_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for the subscribe handshake (connect + ack).
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(15);

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
    next_id: u64,
    /// Cached capabilities (fetched once)
    cached_caps: Option<CapabilityManifest>,
    /// Internal event receiver (from reader task)
    event_rx: Option<mpsc::Receiver<RawEvent>>,
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
            next_id: 1,
            cached_caps: None,
            event_rx: None,
        }
    }

    /// Create adapter from a base socket path.
    ///
    /// Derives command and event socket paths:
    /// - `{base}.sock` for commands
    /// - `{base}-events.sock` for events
    pub fn from_base_path(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        let cmd = PathBuf::from(format!("{}.sock", base.display()));
        let event = PathBuf::from(format!("{}-events.sock", base.display()));
        Self::new(cmd, event)
    }

    /// Send a request and wait for response.
    async fn send(&mut self, request: Request) -> Result<Value> {
        let id = request.id;
        debug!("Sending request: {:?}", request);

        let result = tokio::time::timeout(BRIDGE_CMD_TIMEOUT, async {
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
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(anyhow!(
                "Bridge did not respond within {}s",
                BRIDGE_CMD_TIMEOUT.as_secs()
            )),
        }
    }

    /// Get the next request ID.
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Fetch capabilities from the bridge.
    async fn fetch_capabilities(&mut self) -> Result<CapabilityManifest> {
        let id = self.next_id();
        let result = self.send(Request::caps(id)).await?;

        let caps: Vec<CapabilityInfo> =
            serde_json::from_value(result).context("Invalid capabilities response")?;

        let mut manifest = CapabilityManifest::new();
        for cap in caps {
            // Map access string to capability constructor (case-insensitive)
            let capability = match cap.access.to_lowercase().as_str() {
                "rw" | "read_write" | "readwrite" => Capability::read_write(&cap.name),
                access => {
                    if access != "ro" && access != "read_only" && access != "readonly" {
                        warn!("Unknown access mode '{}' for capability '{}', defaulting to read-only", cap.access, cap.name);
                    }
                    Capability::read_only(&cap.name)
                }
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
        self.cached_caps
            .clone()
            .unwrap_or_default()
    }

    async fn init(&mut self) -> Result<()> {
        // Pre-fetch and cache capabilities from the bridge
        if self.cached_caps.is_none() {
            self.cached_caps = Some(self.fetch_capabilities().await?);
        }
        Ok(())
    }

    async fn subscribe(&mut self, subscriptions: HashSet<String>) -> Result<()> {
        // Connect, send subscribe request, and read ack — all under a single timeout
        let (reader, subscriptions_clone) = tokio::time::timeout(SUBSCRIBE_TIMEOUT, async {
            let stream = UnixStream::connect(&self.event_socket)
                .await
                .with_context(|| format!("Failed to connect to {}", self.event_socket.display()))?;

            debug!("Connected to event socket: {}", self.event_socket.display());

            let events: Vec<&str> = subscriptions.iter().map(|s| s.as_str()).collect();
            let sub_request = Request::subscribe(0, &events);

            let (reader, mut writer) = stream.into_split();

            let mut line = serde_json::to_string(&sub_request)?;
            line.push('\n');
            writer.write_all(line.as_bytes()).await?;

            // Read and validate subscription acknowledgment
            let mut buf_reader = BufReader::new(reader);
            let mut response_line = String::new();
            buf_reader.read_line(&mut response_line).await
                .context("Failed to read subscribe response from bridge")?;
            let response: Response = serde_json::from_str(response_line.trim())
                .context("Invalid subscribe response from bridge")?;
            if let Some(err) = response.error {
                return Err(anyhow!("Bridge rejected subscription: {}", err));
            }
            let reader = buf_reader.into_inner();

            Ok::<_, anyhow::Error>((reader, subscriptions))
        })
        .await
        .map_err(|_| anyhow!(
            "Subscribe handshake timed out after {}s",
            SUBSCRIBE_TIMEOUT.as_secs()
        ))??;

        let subscriptions = subscriptions_clone;

        debug!("Subscribed to events: {:?}", subscriptions);

        // Spawn an internal reader task that pushes events into an mpsc channel.
        // This makes next_event() cancel-safe (read_line isn't cancel-safe).
        let (event_tx, event_rx) = mpsc::channel::<RawEvent>(100);
        self.event_rx = Some(event_rx);

        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                let bytes_read = match reader.read_line(&mut line).await {
                    Ok(n) => n,
                    Err(e) => {
                        error!("Event socket read error: {}", e);
                        break;
                    }
                };

                if bytes_read == 0 {
                    warn!("Event socket closed");
                    break;
                }

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                match serde_json::from_str::<protocol::Event>(trimmed) {
                    Ok(event) => {
                        debug!("Received event: {:?}", event);

                        let mut raw_event = RawEvent::new(event.event);
                        if let Some(prev) = event.prev {
                            raw_event = raw_event.with_prev_state(json_object_to_hashmap(prev));
                        }
                        if let Some(curr) = event.curr {
                            raw_event = raw_event.with_curr_state(json_object_to_hashmap(curr));
                        }

                        if event_tx.send(raw_event).await.is_err() {
                            debug!("Event channel closed");
                            break;
                        }
                    }
                    Err(e) => {
                        if trimmed.contains("\"id\"") {
                            debug!("Skipping response: {}", trimmed);
                        } else {
                            error!("Failed to parse event: {} - {}", e, trimmed);
                        }
                    }
                }
            }
        });

        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<RawEvent>> {
        let rx = self.event_rx.as_mut()
            .ok_or_else(|| anyhow!("subscribe() must be called before next_event()"))?;

        match rx.recv().await {
            Some(event) => Ok(Some(event)),
            None => Ok(None), // Reader task exited
        }
    }

    async fn get(&mut self, key: &str) -> Result<Value> {
        // Ensure caps are cached
        if self.cached_caps.is_none() {
            self.cached_caps = Some(self.fetch_capabilities().await?);
        }

        let id = self.next_id();
        self.send(Request::get(id, key)).await
    }

    async fn set(&mut self, key: &str, value: Value) -> Result<()> {
        // Ensure caps are cached
        if self.cached_caps.is_none() {
            self.cached_caps = Some(self.fetch_capabilities().await?);
        }

        let id = self.next_id();
        self.send(Request::set(id, key, value)).await?;
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Drop event receiver to signal reader task to stop
        self.event_rx = None;
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
