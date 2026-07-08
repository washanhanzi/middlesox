//! Hyprland backend for Middlesox.
//!
//! This crate provides a `ProtocolAdapter` implementation for the
//! Hyprland Wayland compositor using its Unix socket IPC.
//!
//! # Connection
//!
//! Hyprland exposes two sockets under
//! `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`:
//! - `.socket.sock` — request/response commands (what `hyprctl` uses)
//! - `.socket2.sock` — newline-delimited event stream
//!
//! Commands are one connection per request: connect, write the request,
//! read the response until the server closes. Queries use the `j/` prefix
//! for JSON output; state changes use `dispatch`.
//!
//! # Events
//!
//! Events are newline-delimited text in the format `event>>data`, e.g.
//! `workspacev2>>2,web` or `activewindow>>kitty,Terminal`. The adapter
//! tracks last-known state so emitted [`RawEvent`]s carry both `prev`
//! and `curr`, using the same event names as the mangowc backend where
//! the concepts overlap (`focus_change`, `output_focus`, ...) so watch
//! configs are portable between compositors.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use middlesox::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Timeout for command round-trips on `.socket.sock`.
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for connecting to the event socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hyprland protocol adapter.
///
/// Connects to Hyprland's IPC sockets for event listening and command
/// execution.
pub struct HyprlandBackend {
    /// Directory containing `.socket.sock` and `.socket2.sock`.
    socket_dir: PathBuf,
    /// Internal event receiver (from the reader task, set by subscribe).
    event_rx: Option<mpsc::Receiver<RawEvent>>,
}

impl HyprlandBackend {
    /// Create a new Hyprland backend, discovering the socket directory
    /// from `$HYPRLAND_INSTANCE_SIGNATURE` and `$XDG_RUNTIME_DIR`.
    pub fn new() -> Result<Self> {
        Ok(Self {
            socket_dir: discover_socket_dir()?,
            event_rx: None,
        })
    }

    /// Create a backend with an explicit socket directory.
    ///
    /// The directory must contain `.socket.sock` and `.socket2.sock`.
    pub fn with_socket_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            socket_dir: dir.into(),
            event_rx: None,
        }
    }

    fn cmd_socket(&self) -> PathBuf {
        self.socket_dir.join(".socket.sock")
    }

    fn event_socket(&self) -> PathBuf {
        self.socket_dir.join(".socket2.sock")
    }

    /// Send a raw request on the command socket and return the response.
    ///
    /// Mirrors `hyprctl`: one connection per request, write the request,
    /// then read until Hyprland closes the connection.
    async fn command(&self, request: &str) -> Result<String> {
        let path = self.cmd_socket();
        debug!("Hyprland command: {}", request);

        let result = tokio::time::timeout(CMD_TIMEOUT, async {
            let mut stream = UnixStream::connect(&path)
                .await
                .with_context(|| format!("Failed to connect to {}", path.display()))?;

            stream.write_all(request.as_bytes()).await?;

            let mut response = String::new();
            stream.read_to_string(&mut response).await?;
            Ok::<_, anyhow::Error>(response)
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(anyhow!(
                "Hyprland did not respond within {}s",
                CMD_TIMEOUT.as_secs()
            )),
        }
    }

    /// Run a `j/`-prefixed query and parse the JSON response.
    async fn query(&self, cmd: &str) -> Result<Value> {
        let response = self.command(&format!("j/{}", cmd)).await?;
        serde_json::from_str(&response)
            .with_context(|| format!("Invalid JSON from Hyprland for '{}': {}", cmd, response))
    }

    /// Run a dispatcher; Hyprland replies "ok" on success.
    async fn dispatch(&self, args: &str) -> Result<()> {
        let response = self.command(&format!("dispatch {}", args)).await?;
        if response.trim() == "ok" {
            Ok(())
        } else {
            Err(anyhow!("Hyprland dispatch '{}' failed: {}", args, response))
        }
    }

    /// Get the focused monitor from `j/monitors`.
    async fn focused_monitor(&self) -> Result<Value> {
        let monitors = self.query("monitors").await?;
        monitors
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m["focused"].as_bool().unwrap_or(false))
                    .cloned()
            })
            .ok_or_else(|| anyhow!("No focused monitor reported by Hyprland"))
    }
}

/// Discover the Hyprland socket directory from the environment.
fn discover_socket_dir() -> Result<PathBuf> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context(
        "HYPRLAND_INSTANCE_SIGNATURE is not set - is Hyprland running? \
         (set [adapter] socket_dir to override)",
    )?;

    let mut candidates = Vec::new();
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime_dir).join("hypr").join(&signature));
    }
    // Legacy location used by older Hyprland versions
    candidates.push(PathBuf::from("/tmp/hypr").join(&signature));

    candidates
        .into_iter()
        .find(|dir| dir.join(".socket.sock").exists())
        .ok_or_else(|| {
            anyhow!(
                "Hyprland socket not found for instance '{}' \
                 (checked $XDG_RUNTIME_DIR/hypr and /tmp/hypr)",
                signature
            )
        })
}

/// Interpret Hyprland's `fullscreen` field, which is a bool on older
/// versions and a fullscreen-mode integer (0 = none) on newer ones.
fn fullscreen_as_bool(value: &Value) -> bool {
    value.as_bool().unwrap_or_else(|| value.as_i64().unwrap_or(0) != 0)
}

// ============================================================================
// Event translation
// ============================================================================

/// Translates Hyprland `event>>data` lines into [`RawEvent`]s, tracking
/// last-known state so events carry both `prev` and `curr`.
#[derive(Debug, Default)]
struct EventTranslator {
    workspace_id: i64,
    workspace_name: String,
    title: String,
    appid: String,
    output: String,
    fullscreen: bool,
    keymode: String,
    kb_layout: String,
}

fn state_map<const N: usize>(entries: [(&str, Value); N]) -> HashMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

impl EventTranslator {
    /// Translate one line from the event socket. Returns `None` for
    /// unknown events, malformed lines, and no-op transitions.
    ///
    /// `workspace`/`focusedmon`-style v1 events are ignored in favor of
    /// their richer counterparts where Hyprland emits both.
    fn translate(&mut self, line: &str) -> Option<RawEvent> {
        let (name, data) = line.split_once(">>")?;

        match name {
            "workspacev2" => {
                // "ID,NAME"
                let (id, ws_name) = data.split_once(',')?;
                let id: i64 = id.parse().ok()?;
                let prev = state_map([
                    ("id", Value::from(self.workspace_id)),
                    ("name", Value::from(self.workspace_name.clone())),
                ]);
                self.workspace_id = id;
                self.workspace_name = ws_name.to_string();
                let curr = state_map([
                    ("id", Value::from(id)),
                    ("name", Value::from(ws_name)),
                ]);
                Some(
                    RawEvent::new("workspace_change")
                        .with_prev_state(prev)
                        .with_curr_state(curr),
                )
            }
            "activewindow" => {
                // "CLASS,TITLE" (title may contain commas)
                let (class, title) = data.split_once(',')?;
                let prev = state_map([
                    ("title", Value::from(self.title.clone())),
                    ("appid", Value::from(self.appid.clone())),
                ]);
                self.title = title.to_string();
                self.appid = class.to_string();
                let curr = state_map([
                    ("title", Value::from(title)),
                    ("appid", Value::from(class)),
                ]);
                Some(
                    RawEvent::new("focus_change")
                        .with_prev_state(prev)
                        .with_curr_state(curr),
                )
            }
            "focusedmon" => {
                // "MONITOR,WORKSPACENAME"
                let (monitor, workspace) = data.split_once(',')?;
                let prev = state_map([("output", Value::from(self.output.clone()))]);
                self.output = monitor.to_string();
                let curr = state_map([
                    ("output", Value::from(monitor)),
                    ("workspace", Value::from(workspace)),
                ]);
                Some(
                    RawEvent::new("output_focus")
                        .with_prev_state(prev)
                        .with_curr_state(curr),
                )
            }
            "fullscreen" => {
                // "0" or "1"
                let fullscreen = data.trim() == "1";
                let prev = state_map([("fullscreen", Value::from(self.fullscreen))]);
                self.fullscreen = fullscreen;
                let curr = state_map([("fullscreen", Value::from(fullscreen))]);
                Some(
                    RawEvent::new("window_state_change")
                        .with_prev_state(prev)
                        .with_curr_state(curr),
                )
            }
            "submap" => {
                // Submap name; empty means back to the default keymap
                let prev = state_map([("keymode", Value::from(self.keymode.clone()))]);
                self.keymode = data.to_string();
                let curr = state_map([("keymode", Value::from(data))]);
                Some(
                    RawEvent::new("keymode_change")
                        .with_prev_state(prev)
                        .with_curr_state(curr),
                )
            }
            "activelayout" => {
                // "KEYBOARDNAME,LAYOUTNAME"
                let (_, layout) = data.split_once(',')?;
                let prev = state_map([("kb_layout", Value::from(self.kb_layout.clone()))]);
                self.kb_layout = layout.to_string();
                let curr = state_map([("kb_layout", Value::from(layout))]);
                Some(
                    RawEvent::new("keyboard_layout_change")
                        .with_prev_state(prev)
                        .with_curr_state(curr),
                )
            }
            "openwindow" => {
                // "ADDRESS,WORKSPACENAME,CLASS,TITLE" (title may contain commas)
                let mut parts = data.splitn(4, ',');
                let address = parts.next()?;
                let workspace = parts.next()?;
                let class = parts.next()?;
                let title = parts.next()?;
                let curr = state_map([
                    ("address", Value::from(address)),
                    ("workspace", Value::from(workspace)),
                    ("appid", Value::from(class)),
                    ("title", Value::from(title)),
                ]);
                Some(RawEvent::new("window_open").with_curr_state(curr))
            }
            "closewindow" => {
                let prev = state_map([("address", Value::from(data))]);
                Some(RawEvent::new("window_close").with_prev_state(prev))
            }
            "createworkspacev2" => {
                let (id, ws_name) = data.split_once(',')?;
                let id: i64 = id.parse().ok()?;
                let curr = state_map([
                    ("id", Value::from(id)),
                    ("name", Value::from(ws_name)),
                ]);
                Some(RawEvent::new("workspace_create").with_curr_state(curr))
            }
            "destroyworkspacev2" => {
                let (id, ws_name) = data.split_once(',')?;
                let id: i64 = id.parse().ok()?;
                let prev = state_map([
                    ("id", Value::from(id)),
                    ("name", Value::from(ws_name)),
                ]);
                Some(RawEvent::new("workspace_destroy").with_prev_state(prev))
            }
            "monitoradded" => {
                let curr = state_map([("output", Value::from(data))]);
                Some(RawEvent::new("output_add").with_curr_state(curr))
            }
            "monitorremoved" => {
                let prev = state_map([("output", Value::from(data))]);
                Some(RawEvent::new("output_remove").with_prev_state(prev))
            }
            _ => None,
        }
    }
}

// ============================================================================
// ProtocolAdapter implementation
// ============================================================================

#[async_trait]
impl ProtocolAdapter for HyprlandBackend {
    fn name(&self) -> &str {
        "hyprland"
    }

    fn manifest(&self) -> CapabilityManifest {
        CapabilityManifest::new()
            .add(Capability::read_write("workspace").with_description("Active workspace ID"))
            .add(Capability::read_only("workspace_name").with_description("Active workspace name"))
            .add(Capability::read_only("workspaces").with_description("Existing workspaces"))
            .add(Capability::read_only("title").with_description("Focused window title"))
            .add(Capability::read_only("appid").with_description("Focused window class"))
            .add(
                Capability::read_write("fullscreen")
                    .with_description("Focused window fullscreen state"),
            )
            .add(
                Capability::read_write("floating")
                    .with_description("Focused window floating state"),
            )
            .add(Capability::read_only("output").with_description("Focused monitor name"))
            .add(Capability::read_only("outputs").with_description("Connected monitor names"))
            .add(Capability::read_only("client_count").with_description("Number of open windows"))
    }

    async fn init(&mut self) -> Result<()> {
        // Fail fast with a clear error if Hyprland isn't reachable
        let version = self.query("version").await.context(
            "Failed to query Hyprland version - is Hyprland running?",
        )?;
        info!(
            "Connected to Hyprland {}",
            version["tag"].as_str().unwrap_or("(unknown version)")
        );
        Ok(())
    }

    async fn subscribe(&mut self, subscriptions: HashSet<String>) -> Result<()> {
        let path = self.event_socket();
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
            .await
            .map_err(|_| anyhow!("Timed out connecting to {}", path.display()))?
            .with_context(|| format!("Failed to connect to {}", path.display()))?;

        debug!("Connected to Hyprland event socket: {}", path.display());

        // Internal reader task pushes events into an mpsc channel so
        // next_event() is cancel-safe (read_line isn't).
        let (event_tx, event_rx) = mpsc::channel::<RawEvent>(100);
        self.event_rx = Some(event_rx);

        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut translator = EventTranslator::default();
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        warn!("Hyprland event socket closed");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("Hyprland event socket read error: {}", e);
                        break;
                    }
                }

                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }

                // Translate even when unsubscribed so prev/curr state stays current
                let Some(event) = translator.translate(trimmed) else {
                    continue;
                };

                if !subscriptions.contains(&event.name) {
                    continue;
                }

                debug!("Hyprland event: {:?}", event);
                if event_tx.send(event).await.is_err() {
                    debug!("Event channel closed, reader task exiting");
                    break;
                }
            }
        });

        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<RawEvent>> {
        let rx = self
            .event_rx
            .as_mut()
            .ok_or_else(|| anyhow!("subscribe() must be called before next_event()"))?;

        match rx.recv().await {
            Some(event) => Ok(Some(event)),
            None => Ok(None), // Reader task exited
        }
    }

    async fn get(&mut self, key: &str) -> Result<Value> {
        match key {
            "workspace" => Ok(self.query("activeworkspace").await?["id"].clone()),
            "workspace_name" => Ok(self.query("activeworkspace").await?["name"].clone()),
            "workspaces" => {
                let workspaces = self.query("workspaces").await?;
                let list: Vec<Value> = workspaces
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|ws| {
                                serde_json::json!({
                                    "id": ws["id"],
                                    "name": ws["name"],
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Value::from(list))
            }
            // activewindow returns {} when nothing is focused; fall back to defaults
            "title" => Ok(Value::from(
                self.query("activewindow").await?["title"].as_str().unwrap_or(""),
            )),
            "appid" => Ok(Value::from(
                self.query("activewindow").await?["class"].as_str().unwrap_or(""),
            )),
            "fullscreen" => Ok(Value::from(fullscreen_as_bool(
                &self.query("activewindow").await?["fullscreen"],
            ))),
            "floating" => Ok(Value::from(
                self.query("activewindow").await?["floating"].as_bool().unwrap_or(false),
            )),
            "output" => Ok(self.focused_monitor().await?["name"].clone()),
            "outputs" => {
                let monitors = self.query("monitors").await?;
                let names: Vec<String> = monitors
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Value::from(names))
            }
            "client_count" => {
                let clients = self.query("clients").await?;
                Ok(Value::from(
                    clients.as_array().map(|arr| arr.len()).unwrap_or(0) as u64,
                ))
            }
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn set(&mut self, key: &str, value: Value) -> Result<()> {
        match key {
            "workspace" => {
                // Accept an ID or a workspace name
                let target = if let Some(id) = value.as_i64() {
                    id.to_string()
                } else if let Some(name) = value.as_str() {
                    name.to_string()
                } else {
                    return Err(anyhow!("workspace must be a number or string"));
                };
                self.dispatch(&format!("workspace {}", target)).await
            }
            "fullscreen" => {
                let want = value
                    .as_bool()
                    .ok_or_else(|| anyhow!("fullscreen must be a boolean"))?;
                // Hyprland's dispatcher toggles, so only fire when state differs
                let current =
                    fullscreen_as_bool(&self.query("activewindow").await?["fullscreen"]);
                if current != want {
                    self.dispatch("fullscreen 0").await?;
                }
                Ok(())
            }
            "floating" => {
                let want = value
                    .as_bool()
                    .ok_or_else(|| anyhow!("floating must be a boolean"))?;
                let current = self.query("activewindow").await?["floating"]
                    .as_bool()
                    .unwrap_or(false);
                if current != want {
                    self.dispatch("togglefloating").await?;
                }
                Ok(())
            }
            _ => Err(anyhow!("Cannot set '{}' (read-only or unknown)", key)),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Drop the receiver; the reader task exits on its next send
        self.event_rx = None;
        Ok(())
    }
}

/// Create the Hyprland backend with socket discovery from the environment.
pub fn create_backend() -> Result<Box<dyn ProtocolAdapter>> {
    Ok(Box::new(HyprlandBackend::new()?))
}

/// Create the Hyprland backend with an explicit socket directory.
pub fn create_backend_with_socket_dir(dir: impl AsRef<Path>) -> Box<dyn ProtocolAdapter> {
    Box::new(HyprlandBackend::with_socket_dir(dir.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(translator: &mut EventTranslator, line: &str) -> Option<RawEvent> {
        translator.translate(line)
    }

    #[test]
    fn workspace_change_tracks_prev_state() {
        let mut t = EventTranslator::default();

        let first = translate(&mut t, "workspacev2>>2,web").unwrap();
        assert_eq!(first.name, "workspace_change");
        assert_eq!(first.get_prev_i64("id"), Some(0));
        assert_eq!(first.get_curr_i64("id"), Some(2));
        assert_eq!(first.get_curr_str("name"), Some("web"));

        let second = translate(&mut t, "workspacev2>>5,mail").unwrap();
        assert_eq!(second.get_prev_i64("id"), Some(2));
        assert_eq!(second.get_prev_str("name"), Some("web"));
        assert_eq!(second.get_curr_i64("id"), Some(5));
    }

    #[test]
    fn focus_change_maps_class_to_appid_and_keeps_commas_in_title() {
        let mut t = EventTranslator::default();

        let event = translate(&mut t, "activewindow>>kitty,vim: a,b,c.txt").unwrap();
        assert_eq!(event.name, "focus_change");
        assert_eq!(event.get_curr_str("appid"), Some("kitty"));
        assert_eq!(event.get_curr_str("title"), Some("vim: a,b,c.txt"));

        let next = translate(&mut t, "activewindow>>firefox,Mozilla Firefox").unwrap();
        assert_eq!(next.get_prev_str("appid"), Some("kitty"));
        assert_eq!(next.get_prev_str("title"), Some("vim: a,b,c.txt"));
    }

    #[test]
    fn submap_reset_produces_empty_keymode() {
        let mut t = EventTranslator::default();

        let enter = translate(&mut t, "submap>>resize").unwrap();
        assert_eq!(enter.name, "keymode_change");
        assert_eq!(enter.get_curr_str("keymode"), Some("resize"));

        let exit = translate(&mut t, "submap>>").unwrap();
        assert_eq!(exit.get_prev_str("keymode"), Some("resize"));
        assert_eq!(exit.get_curr_str("keymode"), Some(""));
    }

    #[test]
    fn fullscreen_and_layout_events() {
        let mut t = EventTranslator::default();

        let fs = translate(&mut t, "fullscreen>>1").unwrap();
        assert_eq!(fs.name, "window_state_change");
        assert_eq!(fs.get_prev("fullscreen"), Some(&Value::from(false)));
        assert_eq!(fs.get_curr("fullscreen"), Some(&Value::from(true)));

        let layout = translate(&mut t, "activelayout>>at-keyboard,German").unwrap();
        assert_eq!(layout.name, "keyboard_layout_change");
        assert_eq!(layout.get_curr_str("kb_layout"), Some("German"));
    }

    #[test]
    fn open_close_window_events() {
        let mut t = EventTranslator::default();

        let open = translate(&mut t, "openwindow>>abc123,web,kitty,tmux, split").unwrap();
        assert_eq!(open.name, "window_open");
        assert_eq!(open.get_curr_str("address"), Some("abc123"));
        assert_eq!(open.get_curr_str("workspace"), Some("web"));
        assert_eq!(open.get_curr_str("appid"), Some("kitty"));
        assert_eq!(open.get_curr_str("title"), Some("tmux, split"));

        let close = translate(&mut t, "closewindow>>abc123").unwrap();
        assert_eq!(close.name, "window_close");
        assert_eq!(close.get_prev_str("address"), Some("abc123"));
    }

    #[test]
    fn malformed_and_unknown_lines_are_ignored() {
        let mut t = EventTranslator::default();

        assert!(translate(&mut t, "not an event line").is_none());
        assert!(translate(&mut t, "somenewevent>>data").is_none());
        assert!(translate(&mut t, "workspacev2>>notanumber,name").is_none());
        assert!(translate(&mut t, "workspacev2>>missing-comma").is_none());
        // v1 events are ignored in favor of their v2 counterparts
        assert!(translate(&mut t, "workspace>>web").is_none());
    }

    #[test]
    fn fullscreen_field_accepts_bool_and_int() {
        assert!(fullscreen_as_bool(&Value::from(true)));
        assert!(!fullscreen_as_bool(&Value::from(false)));
        assert!(fullscreen_as_bool(&Value::from(2)));
        assert!(!fullscreen_as_bool(&Value::from(0)));
        assert!(!fullscreen_as_bool(&Value::Null));
    }
}
