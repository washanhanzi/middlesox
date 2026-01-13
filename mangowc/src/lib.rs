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

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use middlesox::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
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
        use wayland_backend::protocol::{
            AllowNull, Argument, ArgumentType, Interface, Message, MessageDesc,
        };
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/dwl-ipc-unstable-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/dwl-ipc-unstable-v2.xml");
}

use dwl_ipc::{zdwl_ipc_manager_v2::ZdwlIpcManagerV2, zdwl_ipc_output_v2::ZdwlIpcOutputV2};

/// Map layout codes to human-readable names.
fn layout_code_to_name(code: &str) -> &str {
    match code {
        "S" => "Scroller",
        "T" => "Tile",
        "G" => "Grid",
        "M" => "Monocle",
        "K" | "D" => "Deck",
        "CT" | "C" => "Center Tile",
        "VS" => "Vertical Scroller",
        "VT" => "Vertical Tile",
        "VG" => "Vertical Grid",
        "RT" => "Right Tile",
        "VK" | "VD" => "Vertical Deck",
        "TG" => "TGMix",
        _ => code, // fallback to raw code
    }
}

/// Match either raw layout codes or human-readable layout names.
fn layout_matches(layout_code: &str, requested: &str) -> bool {
    layout_code.eq_ignore_ascii_case(requested)
        || layout_code_to_name(layout_code).eq_ignore_ascii_case(requested)
        || layout_code_to_name(layout_code).eq_ignore_ascii_case(layout_code_to_name(requested))
}

fn value_as_u32(value: &Value, name: &str) -> Result<u32> {
    let raw = value
        .as_u64()
        .ok_or_else(|| anyhow!("{} must be an unsigned integer", name))?;
    u32::try_from(raw).map_err(|_| anyhow!("{} must fit in u32", name))
}

fn parse_tags_value(value: &Value) -> Result<(u32, bool)> {
    if value.is_u64() {
        return Ok((value_as_u32(value, "tags")?, false));
    }

    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("tags must be a number or object"))?;
    let tag_value = object
        .get("tagmask")
        .or_else(|| object.get("tags"))
        .ok_or_else(|| anyhow!("tags object must include tagmask or tags"))?;
    let toggle = object
        .get("toggle")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok((value_as_u32(tag_value, "tagmask")?, toggle))
}

fn parse_client_tags_value(value: &Value) -> Result<(u32, u32)> {
    if value.is_u64() {
        return Ok((0, value_as_u32(value, "client_tags")?));
    }

    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("client_tags must be a number or object"))?;

    if let Some(tags) = object.get("tags") {
        return Ok((0, value_as_u32(tags, "client_tags.tags")?));
    }

    let and_tags = object
        .get("and_tags")
        .ok_or_else(|| anyhow!("client_tags object must include tags or and_tags/xor_tags"))
        .and_then(|value| value_as_u32(value, "and_tags"))?;
    let xor_tags = object
        .get("xor_tags")
        .ok_or_else(|| anyhow!("client_tags object must include tags or and_tags/xor_tags"))
        .and_then(|value| value_as_u32(value, "xor_tags"))?;

    Ok((and_tags, xor_tags))
}

// ============================================================================
// State Types
// ============================================================================

/// State of a single tag on an output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    layout_symbol: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    last_layer: String,
    kb_layout: String,
    keymode: String,
    scale_factor: u32,
}

impl OutputState {
    fn focus_payload(&self, output: impl Into<String>) -> HashMap<String, Value> {
        [
            ("output".to_string(), Value::from(output.into())),
            ("title".to_string(), Value::from(self.title.clone())),
            ("appid".to_string(), Value::from(self.appid.clone())),
        ]
        .into_iter()
        .collect()
    }

    fn occupied_tags(&self) -> u32 {
        self.tag_info
            .iter()
            .enumerate()
            .filter(|(_, info)| info.clients > 0)
            .fold(0u32, |mask, (idx, _)| mask | (1 << idx))
    }

    fn urgent_tags(&self) -> u32 {
        self.tag_info
            .iter()
            .enumerate()
            .filter(|(_, info)| info.state == 2)
            .fold(0u32, |mask, (idx, _)| mask | (1 << idx))
    }

    fn focused_client_tags(&self) -> u32 {
        self.tag_info
            .iter()
            .enumerate()
            .filter(|(_, info)| info.focused)
            .fold(0u32, |mask, (idx, _)| mask | (1 << idx))
    }

    fn client_count(&self) -> u32 {
        self.tag_info.iter().map(|info| info.clients).sum()
    }

    fn layout_label<'a>(&'a self, global: &'a GlobalState) -> &'a str {
        if !self.layout_symbol.is_empty() {
            self.layout_symbol.as_str()
        } else {
            global
                .layouts
                .get(self.layout_idx as usize)
                .map(|s| s.as_str())
                .unwrap_or("")
        }
    }

    fn layout_name(&self, global: &GlobalState) -> String {
        layout_code_to_name(self.layout_label(global)).to_string()
    }

    fn geometry_json(&self) -> Value {
        json!({
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
        })
    }

    fn tag_info_json(&self) -> Value {
        let tags: Vec<Value> = self
            .tag_info
            .iter()
            .enumerate()
            .map(|(idx, info)| {
                json!({
                    "tag": idx + 1,
                    "state": info.state,
                    "clients": info.clients,
                    "focused": info.focused,
                })
            })
            .collect();
        Value::from(tags)
    }
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
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    SetClientTags {
        and_tags: u32,
        xor_tags: u32,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    SetLayout {
        index: u32,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    GetState {
        reply: tokio::sync::oneshot::Sender<CompositorState>,
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
                self.outputs.insert(
                    output_id,
                    (format!("output-{}", output_id), wl_output, Some(ipc_output)),
                );
                debug!("Created IPC output for output {}", output_id);
            }
        }
    }

    /// Send an event if subscribed.
    fn emit_event(
        &self,
        name: &str,
        prev: Option<HashMap<String, Value>>,
        curr: Option<HashMap<String, Value>>,
    ) {
        debug!(
            event = %name,
            subscriptions = ?self.subscriptions,
            subscribed = self.subscriptions.contains(name),
            "emit_event called"
        );

        if !self.subscriptions.contains(name) {
            return;
        }

        let mut event = RawEvent::new(name);
        if let Some(prev) = prev {
            event = event.with_prev_state(prev);
        }
        if let Some(curr) = curr {
            event = event.with_curr_state(curr);
        }

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
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                debug!("Global: {} v{} (name={})", interface, version, name);

                if interface == "zdwl_ipc_manager_v2" {
                    let manager =
                        registry.bind::<ZdwlIpcManagerV2, _, _>(name, version.min(2), qh, ());
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
                        state
                            .outputs
                            .insert(name, (format!("output-{}", name), output, Some(ipc_output)));
                    } else {
                        // Manager not ready yet, store output for later binding
                        state
                            .outputs
                            .insert(name, (format!("output-{}", name), output.clone(), None));
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

                // Get the old name before updating
                let old_name = state.outputs.get(&output_id).map(|(n, _, _)| n.clone());

                // Update the output name
                if let Some((stored_name, _, _)) = state.outputs.get_mut(&output_id) {
                    *stored_name = name.clone();
                }

                // Migrate state from old placeholder name to real name
                if let Some(ref old_name) = old_name
                    && old_name != &name
                {
                    if let Some(pending) = state.pending.remove(old_name) {
                        state.pending.insert(name.clone(), pending);
                    }
                    if let Some(output_state) = state.state.outputs.remove(old_name) {
                        state.state.outputs.insert(name.clone(), output_state);
                    }
                }

                // Initialize state for this output if not already present
                state
                    .state
                    .outputs
                    .entry(name.clone())
                    .or_insert_with(|| OutputState {
                        tag_info: vec![TagInfo::default(); state.state.global.tag_count as usize],
                        ..Default::default()
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
                    output_state
                        .tag_info
                        .resize(amount as usize, TagInfo::default());
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
            state
                .state
                .outputs
                .get(&output_name)
                .cloned()
                .unwrap_or_default()
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
                        let prev = state
                            .state
                            .outputs
                            .get(&prev_output)
                            .map(|output| output.focus_payload(prev_output.clone()))
                            .unwrap_or_else(|| {
                                [("output".to_string(), Value::from(prev_output.clone()))]
                                    .into_iter()
                                    .collect()
                            });
                        let curr = state
                            .state
                            .outputs
                            .get(&output_name)
                            .map(|output| output.focus_payload(output_name.clone()))
                            .unwrap_or_else(|| {
                                [("output".to_string(), Value::from(output_name.clone()))]
                                    .into_iter()
                                    .collect()
                            });

                        state.state.focused_output = output_name.clone();

                        state.emit_event("output_focus", Some(prev), Some(curr));
                    }
                }
            }
            Event::Tag {
                tag,
                state: tag_state,
                clients,
                focused,
            } => {
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
            Event::LayoutSymbol { layout } => {
                pending.layout_symbol = layout;
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
                                Some(
                                    [
                                        ("tags".to_string(), Value::from(prev.active_tags)),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "tags".to_string(),
                                            Value::from(pending_state.active_tags),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.layout_idx != pending_state.layout_idx
                            || prev.layout_symbol != pending_state.layout_symbol
                        {
                            let prev_layout_name = prev.layout_name(&state.state.global);
                            let layout_name = pending_state.layout_name(&state.state.global);
                            state.emit_event(
                                "layout_change",
                                Some(
                                    [
                                        ("layout".to_string(), Value::from(prev.layout_idx)),
                                        ("layout_name".to_string(), Value::from(prev_layout_name)),
                                        (
                                            "layout_symbol".to_string(),
                                            Value::from(prev.layout_symbol.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "layout".to_string(),
                                            Value::from(pending_state.layout_idx),
                                        ),
                                        ("layout_name".to_string(), Value::from(layout_name)),
                                        (
                                            "layout_symbol".to_string(),
                                            Value::from(pending_state.layout_symbol.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.title != pending_state.title || prev.appid != pending_state.appid {
                            state.emit_event(
                                "focus_change",
                                Some(
                                    [
                                        ("title".to_string(), Value::from(prev.title.clone())),
                                        ("appid".to_string(), Value::from(prev.appid.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "title".to_string(),
                                            Value::from(pending_state.title.clone()),
                                        ),
                                        (
                                            "appid".to_string(),
                                            Value::from(pending_state.appid.clone()),
                                        ),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.fullscreen != pending_state.fullscreen
                            || prev.floating != pending_state.floating
                        {
                            state.emit_event(
                                "window_state_change",
                                Some(
                                    [
                                        ("fullscreen".to_string(), Value::from(prev.fullscreen)),
                                        ("floating".to_string(), Value::from(prev.floating)),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "fullscreen".to_string(),
                                            Value::from(pending_state.fullscreen),
                                        ),
                                        (
                                            "floating".to_string(),
                                            Value::from(pending_state.floating),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.x != pending_state.x
                            || prev.y != pending_state.y
                            || prev.width != pending_state.width
                            || prev.height != pending_state.height
                        {
                            state.emit_event(
                                "geometry_change",
                                Some(
                                    [
                                        ("x".to_string(), Value::from(prev.x)),
                                        ("y".to_string(), Value::from(prev.y)),
                                        ("width".to_string(), Value::from(prev.width)),
                                        ("height".to_string(), Value::from(prev.height)),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        ("x".to_string(), Value::from(pending_state.x)),
                                        ("y".to_string(), Value::from(pending_state.y)),
                                        ("width".to_string(), Value::from(pending_state.width)),
                                        ("height".to_string(), Value::from(pending_state.height)),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.last_layer != pending_state.last_layer {
                            state.emit_event(
                                "layer_change",
                                Some(
                                    [
                                        (
                                            "last_layer".to_string(),
                                            Value::from(prev.last_layer.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "last_layer".to_string(),
                                            Value::from(pending_state.last_layer.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.kb_layout != pending_state.kb_layout {
                            state.emit_event(
                                "keyboard_layout_change",
                                Some(
                                    [
                                        (
                                            "kb_layout".to_string(),
                                            Value::from(prev.kb_layout.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "kb_layout".to_string(),
                                            Value::from(pending_state.kb_layout.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.keymode != pending_state.keymode {
                            state.emit_event(
                                "keymode_change",
                                Some(
                                    [
                                        ("keymode".to_string(), Value::from(prev.keymode.clone())),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "keymode".to_string(),
                                            Value::from(pending_state.keymode.clone()),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }

                        if prev.scale_factor != pending_state.scale_factor {
                            state.emit_event(
                                "scale_factor_change",
                                Some(
                                    [
                                        (
                                            "scale_factor".to_string(),
                                            Value::from(prev.scale_factor as f64 / 100.0),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                                Some(
                                    [
                                        (
                                            "scale_factor".to_string(),
                                            Value::from(pending_state.scale_factor as f64 / 100.0),
                                        ),
                                        ("output".to_string(), Value::from(output_name.clone())),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            );
                        }
                    }

                    state.state.outputs.insert(output_name, pending_state);
                }
            }
            Event::X { x } => {
                pending.x = x;
            }
            Event::Y { y } => {
                pending.y = y;
            }
            Event::Width { width } => {
                pending.width = width;
            }
            Event::Height { height } => {
                pending.height = height;
            }
            Event::LastLayer { last_layer } => {
                pending.last_layer = last_layer;
            }
            Event::KbLayout { kb_layout } => {
                pending.kb_layout = kb_layout;
            }
            Event::Keymode { keymode } => {
                pending.keymode = keymode;
            }
            Event::Scalefactor { scalefactor } => {
                pending.scale_factor = scalefactor;
            }
        }
    }
}

// ============================================================================
// Backend
// ============================================================================

/// MangoWC/dwl protocol adapter.
///
/// Connects to dwl-based compositors using native Wayland protocols.
/// The Wayland connection runs in a dedicated blocking thread; get/set
/// communicate with it via a command channel stored in the struct.
pub struct MangoWcBackend {
    /// Command sender to the Wayland thread (initialized on subscribe).
    cmd_tx: Option<std::sync::mpsc::Sender<WaylandCommand>>,
    /// Event receiver from the Wayland thread (initialized on subscribe).
    event_rx: Option<mpsc::Receiver<RawEvent>>,
}

impl MangoWcBackend {
    /// Create a new MangoWC backend.
    pub fn new() -> Result<Self> {
        Ok(Self {
            cmd_tx: None,
            event_rx: None,
        })
    }

    /// Create a backend with explicit display name.
    pub fn with_display(_name: &str) -> Result<Self> {
        // TODO: Support specific display
        Self::new()
    }

    /// Run the blocking Wayland event loop (called from spawn_blocking).
    fn run_wayland_loop(
        cmd_rx: std::sync::mpsc::Receiver<WaylandCommand>,
        event_tx: mpsc::Sender<RawEvent>,
        subscriptions: HashSet<String>,
        init_tx: tokio::sync::oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Helper: on init failure, signal the error back and return it
        macro_rules! try_init {
            ($expr:expr) => {
                match $expr {
                    Ok(val) => val,
                    Err(e) => {
                        let err_msg = format!("{}", e);
                        let _ = init_tx.send(Err(anyhow!("{}", err_msg)));
                        return Err(e);
                    }
                }
            };
        }

        // Connect to Wayland display
        let conn = try_init!(
            Connection::connect_to_env()
                .map_err(|e| anyhow!("Failed to connect to Wayland display: {}", e))
        );

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
        try_init!(
            event_queue
                .roundtrip(&mut wayland_state)
                .map_err(|e| anyhow!("Roundtrip failed: {}", e))
        );

        // Check that we got the manager
        if wayland_state.manager.is_none() {
            let err =
                anyhow!("zdwl_ipc_manager_v2 not available - is this a dwl-based compositor?");
            let _ = init_tx.send(Err(anyhow!(
                "zdwl_ipc_manager_v2 not available - is this a dwl-based compositor?"
            )));
            return Err(err);
        }

        // Another roundtrip to get tags and layouts
        try_init!(
            event_queue
                .roundtrip(&mut wayland_state)
                .map_err(|e| anyhow!("Roundtrip failed: {}", e))
        );

        info!(
            "MangoWC initialized: {} outputs, {} layouts, {} tags",
            wayland_state.state.outputs.len(),
            wayland_state.state.global.layouts.len(),
            wayland_state.state.global.tag_count,
        );

        // Signal successful initialization
        let _ = init_tx.send(Ok(()));

        // Run the event loop
        let fd = conn.as_fd();
        loop {
            // Check for commands (non-blocking)
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    WaylandCommand::SetTags {
                        tagmask,
                        toggle,
                        reply,
                    } => {
                        let result = if let Some(output_id) = wayland_state.focused_output_id {
                            if let Some((_, _, Some(ipc_output))) =
                                wayland_state.outputs.get(&output_id)
                            {
                                ipc_output.set_tags(tagmask, if toggle { 1 } else { 0 });
                                debug!("Set tags: {} (toggle={})", tagmask, toggle);
                                Ok(())
                            } else {
                                Err(anyhow!("No IPC output available for focused output"))
                            }
                        } else {
                            Err(anyhow!("No focused output"))
                        };
                        let _ = reply.send(result);
                    }
                    WaylandCommand::SetClientTags {
                        and_tags,
                        xor_tags,
                        reply,
                    } => {
                        let result = if let Some(output_id) = wayland_state.focused_output_id {
                            if let Some((_, _, Some(ipc_output))) =
                                wayland_state.outputs.get(&output_id)
                            {
                                ipc_output.set_client_tags(and_tags, xor_tags);
                                debug!("Set client tags: and={} xor={}", and_tags, xor_tags);
                                Ok(())
                            } else {
                                Err(anyhow!("No IPC output available for focused output"))
                            }
                        } else {
                            Err(anyhow!("No focused output"))
                        };
                        let _ = reply.send(result);
                    }
                    WaylandCommand::SetLayout { index, reply } => {
                        let result = if let Some(output_id) = wayland_state.focused_output_id {
                            if let Some((_, _, Some(ipc_output))) =
                                wayland_state.outputs.get(&output_id)
                            {
                                ipc_output.set_layout(index);
                                debug!("Set layout: {}", index);
                                Ok(())
                            } else {
                                Err(anyhow!("No IPC output available for focused output"))
                            }
                        } else {
                            Err(anyhow!("No focused output"))
                        };
                        let _ = reply.send(result);
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
                event_queue
                    .blocking_dispatch(&mut wayland_state)
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
        Self {
            cmd_tx: None,
            event_rx: None,
        }
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
            .add(Capability::read_write("layout_name").with_description("Window layout name"))
            .add(
                Capability::read_only("layout_symbol")
                    .with_description("Dynamic window layout symbol"),
            )
            .add(Capability::read_only("layouts").with_description("Available layout names"))
            .add(Capability::read_write("tags").with_description("Active tag bitmask"))
            .add(
                Capability::read_write("client_tags")
                    .with_description("Focused client tag bitmask"),
            )
            .add(Capability::read_only("tag_count").with_description("Number of tags"))
            .add(
                Capability::read_only("tag_info")
                    .with_description("Per-tag state for current output"),
            )
            .add(Capability::read_only("occupied_tags").with_description("Occupied tag bitmask"))
            .add(Capability::read_only("urgent_tags").with_description("Urgent tag bitmask"))
            .add(
                Capability::read_only("client_count")
                    .with_description("Number of clients on current output"),
            )
            .add(Capability::read_only("title").with_description("Focused window title"))
            .add(Capability::read_only("appid").with_description("Focused window app ID"))
            .add(Capability::read_only("output").with_description("Current output name"))
            .add(Capability::read_only("outputs").with_description("Known output names"))
            .add(
                Capability::read_only("fullscreen")
                    .with_description("Focused client fullscreen state"),
            )
            .add(
                Capability::read_only("floating").with_description("Focused client floating state"),
            )
            .add(Capability::read_only("geometry").with_description("Focused client geometry"))
            .add(Capability::read_only("x").with_description("Focused client x coordinate"))
            .add(Capability::read_only("y").with_description("Focused client y coordinate"))
            .add(Capability::read_only("width").with_description("Focused client width"))
            .add(Capability::read_only("height").with_description("Focused client height"))
            .add(Capability::read_only("last_layer").with_description("Last focused layer name"))
            .add(Capability::read_only("kb_layout").with_description("Current keyboard layout"))
            .add(Capability::read_only("keymode").with_description("Current keybind mode"))
            .add(Capability::read_only("scale_factor").with_description("Output scale factor"))
    }

    async fn subscribe(&mut self, subscriptions: HashSet<String>) -> Result<()> {
        debug!(subscriptions = ?subscriptions, "Starting subscribe");

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WaylandCommand>();
        let (event_tx, event_rx) = mpsc::channel::<RawEvent>(100);

        self.cmd_tx = Some(cmd_tx);
        self.event_rx = Some(event_rx);

        // Oneshot to propagate startup success/failure back to subscribe()
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<()>>();

        // Spawn the blocking Wayland event loop in a dedicated thread
        tokio::task::spawn_blocking(move || {
            match Self::run_wayland_loop(cmd_rx, event_tx, subscriptions, init_tx) {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!("Wayland event loop error: {}", e);
                }
            }
        });

        // Wait for the Wayland thread to signal successful initialization
        init_rx
            .await
            .map_err(|_| anyhow!("Wayland thread exited before signaling readiness"))??;

        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<RawEvent>> {
        let rx = self
            .event_rx
            .as_mut()
            .ok_or_else(|| anyhow!("subscribe() must be called before next_event()"))?;

        match rx.recv().await {
            Some(event) => Ok(Some(event)),
            None => Ok(None), // Wayland thread exited
        }
    }

    async fn get(&mut self, key: &str) -> Result<Value> {
        let cmd_tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| anyhow!("Backend not started (subscribe() not called)"))?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        cmd_tx
            .send(WaylandCommand::GetState { reply: reply_tx })
            .map_err(|_| anyhow!("Wayland thread closed"))?;

        let state = reply_rx
            .await
            .map_err(|_| anyhow!("No response from Wayland thread"))?;

        let output = state
            .outputs
            .get(&state.focused_output)
            .ok_or_else(|| anyhow!("No focused output"))?;

        match key {
            "layout" => Ok(Value::from(output.layout_idx)),
            "layout_name" => Ok(Value::from(output.layout_name(&state.global))),
            "layout_symbol" => Ok(Value::from(output.layout_symbol.clone())),
            "layouts" => {
                let names: Vec<String> = state
                    .global
                    .layouts
                    .iter()
                    .map(|code| layout_code_to_name(code).to_string())
                    .collect();
                Ok(Value::from(names))
            }
            "tags" => Ok(Value::from(output.active_tags)),
            "client_tags" => Ok(Value::from(output.focused_client_tags())),
            "tag_count" => Ok(Value::from(state.global.tag_count)),
            "tag_info" => Ok(output.tag_info_json()),
            "occupied_tags" => Ok(Value::from(output.occupied_tags())),
            "urgent_tags" => Ok(Value::from(output.urgent_tags())),
            "client_count" => Ok(Value::from(output.client_count())),
            "title" => Ok(Value::from(output.title.clone())),
            "appid" => Ok(Value::from(output.appid.clone())),
            "output" => Ok(Value::from(state.focused_output.clone())),
            "outputs" => {
                let mut outputs: Vec<String> = state.outputs.keys().cloned().collect();
                outputs.sort();
                Ok(Value::from(outputs))
            }
            "fullscreen" => Ok(Value::from(output.fullscreen)),
            "floating" => Ok(Value::from(output.floating)),
            "geometry" => Ok(output.geometry_json()),
            "x" => Ok(Value::from(output.x)),
            "y" => Ok(Value::from(output.y)),
            "width" => Ok(Value::from(output.width)),
            "height" => Ok(Value::from(output.height)),
            "last_layer" => Ok(Value::from(output.last_layer.clone())),
            "kb_layout" => Ok(Value::from(output.kb_layout.clone())),
            "keymode" => Ok(Value::from(output.keymode.clone())),
            "scale_factor" => Ok(Value::from(output.scale_factor as f64 / 100.0)),
            _ => Err(anyhow!("Unknown key: {}", key)),
        }
    }

    async fn set(&mut self, key: &str, value: Value) -> Result<()> {
        let cmd_tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| anyhow!("Backend not started (subscribe() not called)"))?;

        match key {
            "tags" => {
                let (tagmask, toggle) = parse_tags_value(&value)?;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                cmd_tx
                    .send(WaylandCommand::SetTags {
                        tagmask,
                        toggle,
                        reply: reply_tx,
                    })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                reply_rx
                    .await
                    .map_err(|_| anyhow!("No response from Wayland thread"))?
            }
            "client_tags" => {
                let (and_tags, xor_tags) = parse_client_tags_value(&value)?;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                cmd_tx
                    .send(WaylandCommand::SetClientTags {
                        and_tags,
                        xor_tags,
                        reply: reply_tx,
                    })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                reply_rx
                    .await
                    .map_err(|_| anyhow!("No response from Wayland thread"))?
            }
            "layout" => {
                let index = value
                    .as_u64()
                    .ok_or_else(|| anyhow!("layout must be a number"))?
                    as u32;
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                cmd_tx
                    .send(WaylandCommand::SetLayout {
                        index,
                        reply: reply_tx,
                    })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                reply_rx
                    .await
                    .map_err(|_| anyhow!("No response from Wayland thread"))?
            }
            "layout_name" => {
                let name = value
                    .as_str()
                    .ok_or_else(|| anyhow!("layout_name must be a string"))?;

                // Get current state to find layout index
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                cmd_tx
                    .send(WaylandCommand::GetState { reply: reply_tx })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                let state = reply_rx
                    .await
                    .map_err(|_| anyhow!("No response from Wayland thread"))?;

                // Find the index of the layout code
                let index = state
                    .global
                    .layouts
                    .iter()
                    .position(|code| layout_matches(code, name))
                    .ok_or_else(|| anyhow!("Layout '{}' not found", name))?
                    as u32;

                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                cmd_tx
                    .send(WaylandCommand::SetLayout {
                        index,
                        reply: reply_tx,
                    })
                    .map_err(|_| anyhow!("Wayland thread closed"))?;
                reply_rx
                    .await
                    .map_err(|_| anyhow!("No response from Wayland thread"))?
            }
            _ => Err(anyhow!("Cannot set '{}' (read-only or unknown)", key)),
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        debug!("shutdown() called");
        if let Some(tx) = self.cmd_tx.as_ref() {
            debug!("Sending shutdown command to Wayland thread");
            match tx.send(WaylandCommand::Shutdown) {
                Ok(()) => debug!("Shutdown command sent successfully"),
                Err(e) => warn!("Failed to send shutdown command: {}", e),
            }
        } else {
            debug!("No Wayland state found (backend may not have started)");
        }
        self.cmd_tx = None;
        self.event_rx = None;
        Ok(())
    }
}

/// Create the MangoWC backend.
pub fn create_backend() -> Result<Box<dyn ProtocolAdapter>> {
    Ok(Box::new(MangoWcBackend::new()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use middlesox::AccessMode;

    #[test]
    fn layout_mapping_matches_mango_codes_and_aliases() {
        assert_eq!(layout_code_to_name("K"), "Deck");
        assert_eq!(layout_code_to_name("VK"), "Vertical Deck");
        assert_eq!(layout_code_to_name("TG"), "TGMix");

        assert!(layout_matches("K", "Deck"));
        assert!(layout_matches("D", "K"));
        assert!(layout_matches("VK", "Vertical Deck"));
        assert!(layout_matches("VD", "VK"));
        assert!(layout_matches("TG", "tgmix"));
    }

    #[test]
    fn parse_tags_accepts_bitmask_and_toggle_object() {
        assert_eq!(parse_tags_value(&json!(3)).unwrap(), (3, false));
        assert_eq!(
            parse_tags_value(&json!({ "tagmask": 4, "toggle": true })).unwrap(),
            (4, true)
        );
        assert_eq!(parse_tags_value(&json!({ "tags": 5 })).unwrap(), (5, false));
    }

    #[test]
    fn parse_client_tags_accepts_absolute_and_raw_protocol_masks() {
        assert_eq!(parse_client_tags_value(&json!(3)).unwrap(), (0, 3));
        assert_eq!(
            parse_client_tags_value(&json!({ "tags": 6 })).unwrap(),
            (0, 6)
        );
        assert_eq!(
            parse_client_tags_value(&json!({ "and_tags": 0xfffffff0u32, "xor_tags": 8 })).unwrap(),
            (0xfffffff0, 8)
        );
    }

    #[test]
    fn output_state_derives_tag_and_layout_values() {
        let global = GlobalState {
            tag_count: 3,
            layouts: vec!["S".into(), "T".into()],
        };
        let output = OutputState {
            layout_idx: 1,
            layout_symbol: "TG".into(),
            tag_info: vec![
                TagInfo {
                    state: 1,
                    clients: 2,
                    focused: true,
                },
                TagInfo {
                    state: 2,
                    clients: 1,
                    focused: false,
                },
                TagInfo {
                    state: 0,
                    clients: 0,
                    focused: false,
                },
            ],
            x: 10,
            y: 20,
            width: 800,
            height: 600,
            ..Default::default()
        };

        assert_eq!(output.layout_name(&global), "TGMix");
        assert_eq!(output.occupied_tags(), 0b011);
        assert_eq!(output.urgent_tags(), 0b010);
        assert_eq!(output.focused_client_tags(), 0b001);
        assert_eq!(output.client_count(), 3);
        assert_eq!(
            output.geometry_json(),
            json!({ "x": 10, "y": 20, "width": 800, "height": 600 })
        );
        assert_eq!(
            output.tag_info_json(),
            json!([
                { "tag": 1, "state": 1, "clients": 2, "focused": true },
                { "tag": 2, "state": 2, "clients": 1, "focused": false },
                { "tag": 3, "state": 0, "clients": 0, "focused": false }
            ])
        );
    }

    #[test]
    fn output_state_focus_payload_includes_window_identity() {
        let output = OutputState {
            title: "Terminal".into(),
            appid: "kitty".into(),
            ..Default::default()
        };

        let payload = output.focus_payload("DP-1");

        assert_eq!(payload.get("output"), Some(&Value::from("DP-1")));
        assert_eq!(payload.get("title"), Some(&Value::from("Terminal")));
        assert_eq!(payload.get("appid"), Some(&Value::from("kitty")));
    }

    #[test]
    fn manifest_exposes_mango_status_capabilities() {
        let manifest = MangoWcBackend::default().manifest();

        for key in [
            "layout_symbol",
            "client_tags",
            "tag_count",
            "tag_info",
            "occupied_tags",
            "urgent_tags",
            "client_count",
            "outputs",
            "fullscreen",
            "floating",
            "geometry",
            "last_layer",
            "kb_layout",
            "keymode",
            "scale_factor",
        ] {
            assert!(manifest.find(key).is_some(), "{key} should be exposed");
        }

        assert_eq!(
            manifest.find("client_tags").map(|cap| cap.access),
            Some(AccessMode::ReadWrite)
        );
        assert_eq!(
            manifest.find("scale_factor").map(|cap| cap.access),
            Some(AccessMode::ReadOnly)
        );
    }
}
