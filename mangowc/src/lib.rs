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
use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

// ============================================================================
// Protocol Bindings
// ============================================================================

#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all)]
mod dwl_ipc {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_backend::protocol::{Interface, Message, MessageDesc, ArgumentType, Argument, AllowNull};
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/dwl-ipc-unstable-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/dwl-ipc-unstable-v2.xml");
}

use dwl_ipc::{zdwl_ipc_manager_v2::ZdwlIpcManagerV2, zdwl_ipc_output_v2::ZdwlIpcOutputV2};

// ============================================================================
// State Types
// ============================================================================

/// State of a single tag on an output.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct TagInfo {
    state: u32,    // 0=none, 1=active, 2=urgent
    clients: u32,  // number of clients on this tag
    focused: bool, // has focused client
}

/// Per-output state from zdwl_ipc_output_v2.
#[derive(Debug, Clone, Default)]
struct OutputState {
    // Tracked (manifest capabilities)
    active_tags: u32, // bitmask of currently viewed tags -> "tags" capability
    layout_idx: u32,  // current layout index -> "layout" capability
    title: String,    // focused client title -> "title" capability
    appid: String,    // focused client app ID -> "appid" capability

    // Tag metadata (not directly exposed, used for events)
    tag_info: Vec<TagInfo>,

    // Additional state
    active: bool,
    fullscreen: bool,
    floating: bool,
}

/// Global state from zdwl_ipc_manager_v2.
#[derive(Debug, Clone, Default)]
struct GlobalState {
    tag_count: u32,
    layouts: Vec<String>,
}

/// Combined compositor state.
#[derive(Debug, Default)]
struct CompositorState {
    global: GlobalState,
    outputs: HashMap<String, OutputState>,
    focused_output: String,
}

// ============================================================================
// Wayland State
// ============================================================================

/// Commands that can be sent from async code to the Wayland thread.
enum WaylandCommand {
    SetTags {
        tagmask: u32,
        toggle: bool,
    },
    SetLayout {
        index: u32,
    },
    GetState {
        reply: std::sync::mpsc::Sender<CompositorState>,
    },
    Shutdown,
}

/// State for the Wayland event loop.
struct WaylandState {
    /// The zdwl_ipc_manager global.
    manager: Option<ZdwlIpcManagerV2>,

    /// Mapping from wl_output to (output_name, wl_output, zdwl_ipc_output).
    outputs: HashMap<u32, (String, WlOutput, Option<ZdwlIpcOutputV2>)>,

    /// Outputs discovered before manager was ready (need IPC binding).
    pending_outputs: Vec<(u32, WlOutput)>,

    /// Current compositor state.
    state: CompositorState,

    /// Pending state updates (double-buffered).
    pending: HashMap<String, OutputState>,

    /// Channel to send events to the async layer.
    event_tx: mpsc::Sender<RawEvent>,

    /// Subscribed event types.
    subscriptions: HashSet<String>,

    /// Track the focused output (by wl_output id).
    focused_output_id: Option<u32>,
}

impl WaylandState {
    fn new(event_tx: mpsc::Sender<RawEvent>, subscriptions: HashSet<String>) -> Self {
        Self {
            manager: None,
            outputs: HashMap::new(),
            pending_outputs: Vec::new(),
            state: CompositorState::default(),
            pending: HashMap::new(),
            event_tx,
            subscriptions,
            focused_output_id: None,
        }
    }

    /// Bind IPC outputs for any pending wl_outputs (called after manager is available).
    fn bind_pending_outputs(&mut self, qh: &QueueHandle<Self>) {
        if self.manager.is_none() {
            return;
        }

        let pending = std::mem::take(&mut self.pending_outputs);
        for (output_id, wl_output) in pending {
            if let Some(manager) = &self.manager {
                let ipc_output = manager.get_output(&wl_output, qh, output_id);
                self.outputs.insert(output_id, (format!("output-{}", output_id), wl_output, Some(ipc_output)));
                debug!("Created IPC output for output {}", output_id);
            }
        }
    }

    /// Send an event if subscribed.
    fn emit_event(&self, name: &str, prev: Option<HashMap<String, Value>>, curr: Option<HashMap<String, Value>>) {
        debug!(
            event = %name,
            subscriptions = ?self.subscriptions,
            subscribed = self.subscriptions.contains(name),
            "emit_event called"
        );

        if !self.subscriptions.contains(name) {
            return;
        }

        let event = RawEvent {
            name: name.to_string(),
            prev,
            curr,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };

        debug!(event = %name, "Sending event to channel");
        if let Err(e) = self.event_tx.try_send(event) {
            warn!("Failed to send event: {}", e);
        }
    }
}

// ============================================================================
// Wayland Dispatch Implementations
// ============================================================================

impl Dispatch<WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                debug!("Global: {} v{} (name={})", interface, version, name);

                if interface == "zdwl_ipc_manager_v2" {
                    let manager = registry.bind::<ZdwlIpcManagerV2, _, _>(name, version.min(2), qh, ());
                    state.manager = Some(manager);
                    info!("Bound to zdwl_ipc_manager_v2");

                    // Bind any pending outputs now that manager is available
                    state.bind_pending_outputs(qh);
                } else if interface == "wl_output" {
                    let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, name);

                    // If manager is ready, create IPC output immediately
                    // Otherwise, defer to pending_outputs
                    if let Some(manager) = &state.manager {
                        let ipc_output = manager.get_output(&output, qh, name);
                        state.outputs.insert(name, (format!("output-{}", name), output, Some(ipc_output)));
                    } else {
                        // Manager not ready yet, store output for later binding
                        state.outputs.insert(name, (format!("output-{}", name), output.clone(), None));
                        state.pending_outputs.push((name, output));
                        debug!("Deferring IPC binding for output {}", name);
                    }
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if state.outputs.contains_key(&name) {
                    let (output_name, _, _) = state.outputs.remove(&name).unwrap();
                    state.state.outputs.remove(&output_name);
                    info!("Output removed: {}", output_name);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _output: &WlOutput,
        event: wayland_client::protocol::wl_output::Event,
        data: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;
        match event {
            Event::Name { name } => {
                let output_id = *data;
                debug!("Output {} name: {}", output_id, name);

                // Update the output name
                if let Some((stored_name, _, _)) = state.outputs.get_mut(&output_id) {
                    *stored_name = name.clone();
                }

                // Initialize state for this output
                state.state.outputs.entry(name.clone()).or_insert_with(|| {
                    OutputState {
                        tag_info: vec![TagInfo::default(); state.state.global.tag_count as usize],
                        ..Default::default()
                    }
                });

                // Set as focused if it's the first output
                if state.state.focused_output.is_empty() {
                    state.state.focused_output = name.clone();
                    state.focused_output_id = Some(output_id);
                }

                // Now bind the IPC output if we have the manager and haven't bound yet
                if let Some(manager) = &state.manager {
                    if let Some((_, wl_output, ipc_opt)) = state.outputs.get(&output_id) {
                        if ipc_opt.is_none() {
                            let ipc_output = manager.get_output(wl_output, qh, output_id);
                            if let Some((_, _, old_ipc)) = state.outputs.get_mut(&output_id) {
                                *old_ipc = Some(ipc_output);
                            }
                        }
                    }
                }
            }
            Event::Done => {
                debug!("Output done event");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZdwlIpcManagerV2, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _manager: &ZdwlIpcManagerV2,
        event: dwl_ipc::zdwl_ipc_manager_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use dwl_ipc::zdwl_ipc_manager_v2::Event;
        match event {
            Event::Tags { amount } => {
                debug!("Manager: {} tags", amount);
                state.state.global.tag_count = amount;

                // Initialize tag_info for all outputs
                for output_state in state.state.outputs.values_mut() {
                    output_state.tag_info.resize(amount as usize, TagInfo::default());
                }
            }
            Event::Layout { name } => {
                debug!("Manager: layout '{}'", name);
                state.state.global.layouts.push(name);
            }
        }
    }
}

impl Dispatch<ZdwlIpcOutputV2, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _output: &ZdwlIpcOutputV2,
        event: dwl_ipc::zdwl_ipc_output_v2::Event,
        data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let output_id = *data;

        // Get the output name
        let output_name = match state.outputs.get(&output_id) {
            Some((name, _, _)) => name.clone(),
            None => return,
        };


        // Get or create pending state for this output
        let pending = state.pending.entry(output_name.clone()).or_insert_with(|| {
            state.state.outputs.get(&output_name).cloned().unwrap_or_default()
        });

        use dwl_ipc::zdwl_ipc_output_v2::Event;
        match event {
            Event::Active { active } => {
                let is_active = active != 0;
                pending.active = is_active;

                // Update focused output
                if is_active {
                    state.focused_output_id = Some(output_id);
                    if state.state.focused_output != output_name {
                        let prev_output = state.state.focused_output.clone();
                        state.state.focused_output = output_name.clone();

                        state.emit_event(
                            "output_focus",
                            Some([("output".to_string(), Value::from(prev_output))].into_iter().collect()),
                            Some([("output".to_string(), Value::from(output_name.clone()))].into_iter().collect()),
                        );
                    }
                }
            }
            Event::Tag { tag, state: tag_state, clients, focused } => {
                // Convert WEnum<TagState> to u32
                let state_val = match tag_state {
                    wayland_client::WEnum::Value(v) => v as u32,
                    wayland_client::WEnum::Unknown(v) => v as u32,
                };
                if (tag as usize) < pending.tag_info.len() {
                    pending.tag_info[tag as usize] = TagInfo {
                        state: state_val,
                        clients,
                        focused: focused != 0,
                    };

                    // Calculate active_tags bitmask
                    let mut active_tags = 0u32;
                    for (i, info) in pending.tag_info.iter().enumerate() {
                        if info.state == 1 {
                            // active
                            active_tags |= 1 << i;
                        }
                    }
                    pending.active_tags = active_tags;
                }
            }
            Event::Layout { layout } => {
                pending.layout_idx = layout;
            }
            Event::Title { title } => {
                pending.title = title;
            }
            Event::Appid { appid } => {
                pending.appid = appid;
            }
            Event::Fullscreen { is_fullscreen } => {
                pending.fullscreen = is_fullscreen != 0;
            }
            Event::Floating { is_floating } => {
                pending.floating = is_floating != 0;
            }
            Event::LayoutSymbol { .. } => {
                // We use layout index instead
            }
            Event::ToggleVisibility => {
                // Not relevant for status tracking
            }
            Event::Frame => {
                // Apply pending state
                if let Some(pending_state) = state.pending.remove(&output_name) {
                    let prev_state = state.state.outputs.get(&output_name);

                    // Emit events for changes
                    if let Some(prev) = prev_state {
                        if prev.active_tags != pending_state.active_tags {
                            state.emit_event(
                                "tag_change",
                                Some([
                                    ("tags".to_string(), Value::from(prev.active_tags)),
                                    ("output".to_string(), Value::from(output_name.clone())),
                                ].into_iter().collect()),
                                Some([
                                    ("tags".to_string(), Value::from(pending_state.active_tags)),
                                    ("output".to_string(), Value::from(output_name.clone())),
                                ].into_iter().collect()),
                            );
                        }

                        if prev.layout_idx != pending_state.layout_idx {
                            let layout_name = state.state.global.layouts
                                .get(pending_state.layout_idx as usize)
                                .cloned()
                                .unwrap_or_default();
                            state.emit_event(
                                "layout_change",
                                Some([
                                    ("layout".to_string(), Value::from(prev.layout_idx)),
                                    ("output".to_string(), Value::from(output_name.clone())),
                                ].into_iter().collect()),
                                Some([
                                    ("layout".to_string(), Value::from(pending_state.layout_idx)),
                                    ("layout_name".to_string(), Value::from(layout_name)),
                                    ("output".to_string(), Value::from(output_name.clone())),
                                ].into_iter().collect()),
                            );
                        }

                        if prev.title != pending_state.title || prev.appid != pending_state.appid {
                            state.emit_event(
                                "focus_change",
                                Some([
                                    ("title".to_string(), Value::from(prev.title.clone())),
                                    ("appid".to_string(), Value::from(prev.appid.clone())),
                                ].into_iter().collect()),
                                Some([
                                    ("title".to_string(), Value::from(pending_state.title.clone())),
                                    ("appid".to_string(), Value::from(pending_state.appid.clone())),
                                ].into_iter().collect()),
                            );
                        }
                    }

                    state.state.outputs.insert(output_name, pending_state);
                }
            }
            // MangoWC extended events - we track them but don't emit events for now
            Event::X { .. } => {}
            Event::Y { .. } => {}
            Event::Width { .. } => {}
            Event::Height { .. } => {}
            Event::LastLayer { .. } => {}
            Event::KbLayout { .. } => {}
            Event::Keymode { .. } => {}
            Event::Scalefactor { .. } => {}
        }
    }
}

// ============================================================================
// Shared Wayland State (Singleton)
// ============================================================================

use std::sync::OnceLock;

/// Shared state for the Wayland connection, accessible by all backend instances.
struct SharedWaylandState {
    cmd_tx: std::sync::mpsc::Sender<WaylandCommand>,
}

static WAYLAND_STATE: OnceLock<Mutex<Option<SharedWaylandState>>> = OnceLock::new();

fn get_wayland_state() -> &'static Mutex<Option<SharedWaylandState>> {
    WAYLAND_STATE.get_or_init(|| Mutex::new(None))
}

// ============================================================================
// Backend
// ============================================================================

/// MangoWC/dwl protocol adapter.
///
/// Connects to dwl-based compositors using native Wayland protocols.
/// All instances share the same Wayland connection via a global singleton.
pub struct MangoWcBackend;

impl MangoWcBackend {
    /// Create a new MangoWC backend.
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    /// Create a backend with explicit display name.
    pub fn with_display(_name: &str) -> Result<Self> {
        // TODO: Support specific display
        Self::new()
    }

    /// Run the blocking Wayland event loop (called from spawn_blocking).
    fn run_wayland_loop(
        event_tx: mpsc::Sender<RawEvent>,
        subscriptions: HashSet<String>,
    ) -> Result<()> {
        // Connect to Wayland display
        let conn = Connection::connect_to_env()
            .map_err(|e| anyhow!("Failed to connect to Wayland display: {}", e))?;

        info!("Connected to Wayland display");

        // Create event queue
        let mut event_queue: EventQueue<WaylandState> = conn.new_event_queue();
        let qh = event_queue.handle();

        // Create state
        let mut wayland_state = WaylandState::new(event_tx, subscriptions);

        // Get registry and do initial roundtrip
        let display = conn.display();
        display.get_registry(&qh, ());

        // Roundtrip to get globals
        event_queue.roundtrip(&mut wayland_state)
            .map_err(|e| anyhow!("Roundtrip failed: {}", e))?;

        // Check that we got the manager
        if wayland_state.manager.is_none() {
            return Err(anyhow!("zdwl_ipc_manager_v2 not available - is this a dwl-based compositor?"));
        }

        // Another roundtrip to get tags and layouts
        event_queue.roundtrip(&mut wayland_state)
            .map_err(|e| anyhow!("Roundtrip failed: {}", e))?;

        info!(
            "MangoWC initialized: {} outputs, {} layouts, {} tags",
            wayland_state.state.outputs.len(),
            wayland_state.state.global.layouts.len(),
            wayland_state.state.global.tag_count,
        );

        // Create command channel
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WaylandCommand>();

        // Store the sender in the global shared state
        {
            let mut guard = get_wayland_state().lock().unwrap();
            *guard = Some(SharedWaylandState { cmd_tx });
        }

        // Run the event loop
        let fd = conn.as_fd();
        loop {
            // Check for commands (non-blocking)
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    WaylandCommand::SetTags { tagmask, toggle } => {
                        if let Some(output_id) = wayland_state.focused_output_id {
                            if let Some((_, _, Some(ipc_output))) = wayland_state.outputs.get(&output_id) {
                                ipc_output.set_tags(tagmask, if toggle { 1 } else { 0 });
                                debug!("Set tags: {} (toggle={})", tagmask, toggle);
                            }
                        }
                    }
                    WaylandCommand::SetLayout { index } => {
                        if let Some(output_id) = wayland_state.focused_output_id {
                            if let Some((_, _, Some(ipc_output))) = wayland_state.outputs.get(&output_id) {
                                ipc_output.set_layout(index);
                                debug!("Set layout: {}", index);
                            }
                        }
                    }
                    WaylandCommand::GetState { reply } => {
                        let state_clone = CompositorState {
                            global: wayland_state.state.global.clone(),
                            outputs: wayland_state.state.outputs.clone(),
                            focused_output: wayland_state.state.focused_output.clone(),
                        };
                        let _ = reply.send(state_clone);
                    }
                    WaylandCommand::Shutdown => {
                        info!("MangoWC backend received shutdown command, exiting event loop");
                        return Ok(());
                    }
                }
            }

            // Flush outgoing requests
            conn.flush().map_err(|e| anyhow!("Flush failed: {}", e))?;

            // Wait for events with timeout (to check commands periodically)
            use std::os::unix::io::AsRawFd;
            let mut pollfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pollfd, 1, 100) }; // 100ms timeout

            if ret > 0 {
                // Dispatch events
                event_queue.blocking_dispatch(&mut wayland_state)
                    .map_err(|e| anyhow!("Dispatch failed: {}", e))?;
            } else if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(anyhow!("Poll failed: {}", err));
                }
            }
        }
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
        CapabilityManifest::new()
            .add(Capability::read_write("layout").with_description("Window layout index"))
            .add(Capability::read_write("tags").with_description("Active tag bitmask"))
            .add(Capability::read_only("title").with_description("Focused window title"))
            .add(Capability::read_only("appid").with_description("Focused window app ID"))
            .add(Capability::read_only("output").with_description("Current output name"))
    }

    async fn listen(
        &self,
        event_tx: mpsc::Sender<RawEvent>,
        subscriptions: HashSet<String>,
    ) -> Result<()> {
        debug!(subscriptions = ?subscriptions, "Starting listen with subscriptions");

        // Run the blocking Wayland event loop in a dedicated thread
        let result = tokio::task::spawn_blocking(move || {
            Self::run_wayland_loop(event_tx, subscriptions)
        }).await;

        match result {
            Ok(inner) => inner,
            Err(e) => Err(anyhow!("Wayland task panicked: {}", e)),
        }
    }

    async fn get(&self, key: &str) -> Result<Value> {
        let cmd_tx = {
            let guard = get_wayland_state().lock().unwrap();
            guard.as_ref()
                .map(|s| s.cmd_tx.clone())
                .ok_or_else(|| anyhow!("Backend not started (listen() not called)"))?
        };

        // Use std::sync::mpsc for synchronous reply (Wayland loop is blocking)
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        cmd_tx.send(WaylandCommand::GetState { reply: reply_tx })
            .map_err(|_| anyhow!("Wayland thread closed"))?;

        let state = reply_rx.recv().map_err(|_| anyhow!("No response from Wayland thread"))?;

        let output = state
            .outputs
            .get(&state.focused_output)
            .ok_or_else(|| anyhow!("No focused output"))?;

        match key {
            "layout" => Ok(Value::from(output.layout_idx)),
            "tags" => Ok(Value::from(output.active_tags)),
            "title" => Ok(Value::from(output.title.clone())),
            "appid" => Ok(Value::from(output.appid.clone())),
            "output" => Ok(Value::from(state.focused_output.clone())),
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn set(&self, key: &str, value: Value) -> Result<()> {
        let cmd_tx = {
            let guard = get_wayland_state().lock().unwrap();
            guard.as_ref()
                .map(|s| s.cmd_tx.clone())
                .ok_or_else(|| anyhow!("Backend not started (listen() not called)"))?
        };

        match key {
            "tags" => {
                let tagmask = value.as_u64()
                    .ok_or_else(|| anyhow!("tags must be a number"))? as u32;
                cmd_tx.send(WaylandCommand::SetTags { tagmask, toggle: false })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                Ok(())
            }
            "layout" => {
                let index = value.as_u64()
                    .ok_or_else(|| anyhow!("layout must be a number"))? as u32;
                cmd_tx.send(WaylandCommand::SetLayout { index })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                Ok(())
            }
            _ => Err(anyhow!("Cannot set '{}' (read-only or unknown)", key)),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        debug!("shutdown() called");
        let guard = get_wayland_state().lock().unwrap();
        if let Some(state) = guard.as_ref() {
            debug!("Sending shutdown command to Wayland thread");
            match state.cmd_tx.send(WaylandCommand::Shutdown) {
                Ok(()) => debug!("Shutdown command sent successfully"),
                Err(e) => warn!("Failed to send shutdown command: {}", e),
            }
        } else {
            debug!("No Wayland state found (backend may not have started)");
        }
        drop(guard);
        Ok(())
    }
}

/// Create the MangoWC backend.
pub fn create_backend() -> Result<Box<dyn ProtocolAdapter>> {
    Ok(Box::new(MangoWcBackend::new()?))
}
