//! Rhai scripting engine with security enforcement.
//!
//! This module wraps the Rhai scripting engine and provides secure
//! access to window manager state through the adapter handle.

use crate::adapter_handle::AdapterHandle;
use crate::RawEvent;
use anyhow::{anyhow, Result};
use rhai::{Dynamic, Engine, Scope};
use serde_json::Value;
use std::collections::HashMap;
use tracing::{debug, error, warn};

/// Event context passed to scripts.
#[derive(Debug, Clone)]
struct EventContext {
    event_name: String,
    prev: HashMap<String, Value>,
    curr: HashMap<String, Value>,
}

/// Security-enforced script engine for window manager control.
///
/// Wraps Rhai and validates all operations against the capability
/// manifest before allowing them to execute. The manifest is fetched
/// from the adapter handle on each security check.
pub struct ScriptEngine {
    adapter: AdapterHandle,
}

impl ScriptEngine {
    /// Create a new script engine with the given adapter handle.
    pub fn new(adapter: AdapterHandle) -> Self {
        Self { adapter }
    }

    fn create_engine() -> Engine {
        let mut engine = Engine::new();

        // Disable potentially dangerous operations
        engine.set_max_expr_depths(64, 64);
        engine.set_max_string_size(10_000);
        engine.set_max_array_size(1_000);
        engine.set_max_map_size(500);
        engine.set_max_operations(100_000);

        engine
    }

    /// Execute a script with access to get/set functions.
    ///
    /// The script can use:
    /// - `get("key")` - Query a value from the window manager
    /// - `set("key", value)` - Set a value in the window manager (security-checked).
    ///   Returns an empty string on success or an error message on failure.
    pub async fn execute(&self, script: &str) -> Result<Dynamic> {
        self.execute_with_context(script, None).await
    }

    /// Execute a script with event context.
    ///
    /// The script can use:
    /// - `get("key")` - Query a value from the window manager
    /// - `set("key", value)` - Set a value in the window manager (security-checked).
    ///   Returns an empty string on success or an error message on failure.
    /// - `event_name` - Name of the event that triggered the script
    /// - `prev` - Previous state (map)
    /// - `curr` - Current state (map)
    pub async fn execute_with_event(&self, script: &str, event: &RawEvent) -> Result<Dynamic> {
        let context = EventContext {
            event_name: event.name.clone(),
            prev: event.prev.clone().unwrap_or_default(),
            curr: event.curr.clone().unwrap_or_default(),
        };
        self.execute_with_context(script, Some(context)).await
    }

    async fn execute_with_context(&self, script: &str, context: Option<EventContext>) -> Result<Dynamic> {
        let adapter = self.adapter.clone();
        let script = script.to_string();

        // Run the script in a blocking task since Rhai isn't async
        let result = tokio::task::spawn_blocking(move || {
            Self::execute_sync(&script, adapter, context)
        })
        .await
        .map_err(|e| anyhow!("Script task panicked: {}", e))??;

        Ok(result)
    }

    fn execute_sync(
        script: &str,
        adapter: AdapterHandle,
        context: Option<EventContext>,
    ) -> Result<Dynamic> {
        let rt = tokio::runtime::Handle::current();

        let mut engine = Self::create_engine();
        let mut scope = Scope::new();

        // Inject event context if provided
        if let Some(ctx) = context {
            scope.push("event_name", ctx.event_name);
            scope.push("prev", json_map_to_dynamic(ctx.prev));
            scope.push("curr", json_map_to_dynamic(ctx.curr));
        }

        // Clone for closures
        let adapter_get = adapter.clone();
        let adapter_set = adapter.clone();

        // Register the `get` function
        engine.register_fn("get", move |key: &str| -> Dynamic {
            let adapter = adapter_get.clone();
            let key = key.to_string();

            rt.block_on(async {
                match adapter.get(&key).await {
                    Ok(value) => json_to_dynamic(value),
                    Err(e) => {
                        error!("Script get('{}') failed: {}", key, e);
                        Dynamic::UNIT
                    }
                }
            })
        });

        // Register the `set` function with security enforcement.
        // Returns "" on success or an error message string on failure.
        let rt2 = tokio::runtime::Handle::current();
        engine.register_fn("set", move |key: &str, value: Dynamic| -> String {
            let adapter = adapter_set.clone();
            let key = key.to_string();

            // Fetch manifest from adapter
            let manifest = match rt2.block_on(adapter.manifest()) {
                Ok(m) => m,
                Err(e) => {
                    error!("Script set('{}') failed to get manifest: {}", key, e);
                    return format!("failed to get manifest: {}", e);
                }
            };

            // Security check: validate against manifest
            let capability = match manifest.find(&key) {
                Some(cap) => cap.clone(),
                None => {
                    warn!("Script tried to set unknown key: {}", key);
                    return format!("unknown key: {}", key);
                }
            };

            if !capability.is_writable() {
                warn!(
                    "Security violation: script tried to write read-only key '{}'",
                    key
                );
                return format!("read-only key: {}", key);
            }

            let json_value = dynamic_to_json(value);

            rt2.block_on(async {
                match adapter.set(&key, json_value).await {
                    Ok(()) => {
                        debug!("Script set('{}') succeeded", key);
                        String::new()
                    }
                    Err(e) => {
                        error!("Script set('{}') failed: {}", key, e);
                        format!("adapter error: {}", e)
                    }
                }
            })
        });

        // Register logging functions
        engine.register_fn("print", |msg: &str| {
            debug!("Script: {}", msg);
        });

        engine.register_fn("log", |msg: &str| {
            debug!("Script log: {}", msg);
        });

        // Run the script
        let result = engine.eval_with_scope::<Dynamic>(&mut scope, script);

        match result {
            Ok(val) => Ok(val),
            Err(e) => Err(anyhow!("Script error: {}", e)),
        }
    }

    /// Execute a script file.
    pub async fn execute_file(&self, path: &std::path::Path) -> Result<Dynamic> {
        let script = tokio::fs::read_to_string(path).await?;
        self.execute(&script).await
    }

    /// Execute a script file with event context.
    pub async fn execute_file_with_event(&self, path: &std::path::Path, event: &RawEvent) -> Result<Dynamic> {
        let script = tokio::fs::read_to_string(path).await?;
        self.execute_with_event(&script, event).await
    }
}

/// Convert a serde_json::Value to a Rhai Dynamic.
fn json_to_dynamic(value: Value) -> Dynamic {
    match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::UNIT
            }
        }
        Value::String(s) => Dynamic::from(s),
        Value::Array(arr) => {
            let vec: Vec<Dynamic> = arr.into_iter().map(json_to_dynamic).collect();
            Dynamic::from(vec)
        }
        Value::Object(obj) => {
            let map: rhai::Map = obj
                .into_iter()
                .map(|(k, v)| (k.into(), json_to_dynamic(v)))
                .collect();
            Dynamic::from(map)
        }
    }
}

/// Convert a HashMap<String, Value> to a Rhai Dynamic map.
fn json_map_to_dynamic(map: HashMap<String, Value>) -> Dynamic {
    let rhai_map: rhai::Map = map
        .into_iter()
        .map(|(k, v)| (k.into(), json_to_dynamic(v)))
        .collect();
    Dynamic::from(rhai_map)
}

/// Convert a Rhai Dynamic to a serde_json::Value.
fn dynamic_to_json(value: Dynamic) -> Value {
    if value.is_unit() {
        Value::Null
    } else if value.is_bool() {
        Value::Bool(value.as_bool().unwrap())
    } else if value.is_int() {
        Value::Number(value.as_int().unwrap().into())
    } else if value.is_float() {
        if let Some(f) = serde_json::Number::from_f64(value.as_float().unwrap()) {
            Value::Number(f)
        } else {
            Value::Null
        }
    } else if value.is_string() {
        Value::String(value.into_string().unwrap())
    } else if value.is_array() {
        let arr: Vec<Dynamic> = value.into_typed_array().unwrap_or_default();
        Value::Array(arr.into_iter().map(dynamic_to_json).collect())
    } else if value.is_map() {
        let map: rhai::Map = value.cast();
        let obj: serde_json::Map<String, Value> = map
            .into_iter()
            .map(|(k, v)| (k.to_string(), dynamic_to_json(v)))
            .collect();
        Value::Object(obj)
    } else {
        Value::Null
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capability, CapabilityManifest, ProtocolAdapter, RawEvent};
    use std::collections::HashSet;

    /// Minimal in-crate mock for engine tests.
    struct TestBackend {
        workspace: i64,
        layout: String,
    }

    impl TestBackend {
        fn new() -> Self {
            Self {
                workspace: 1,
                layout: "master".into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ProtocolAdapter for TestBackend {
        fn name(&self) -> &str { "test" }

        fn manifest(&self) -> CapabilityManifest {
            CapabilityManifest::new()
                .add(Capability::read_write("workspace"))
                .add(Capability::read_write("layout"))
                .add(Capability::read_only("monitor"))
        }

        async fn subscribe(&mut self, _subs: HashSet<String>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> anyhow::Result<Option<RawEvent>> {
            // Never produces events — just pend forever
            std::future::pending().await
        }

        async fn get(&mut self, key: &str) -> anyhow::Result<Value> {
            match key {
                "workspace" => Ok(Value::from(self.workspace)),
                "layout" => Ok(Value::from(self.layout.clone())),
                "monitor" => Ok(Value::from("TEST-1")),
                _ => Err(anyhow!("Unknown key: {}", key)),
            }
        }

        async fn set(&mut self, key: &str, value: Value) -> anyhow::Result<()> {
            match key {
                "workspace" => {
                    self.workspace = value.as_i64().unwrap();
                    Ok(())
                }
                "layout" => {
                    self.layout = value.as_str().unwrap().to_string();
                    Ok(())
                }
                _ => Err(anyhow!("Read-only or unknown key: {}", key)),
            }
        }
    }

    async fn test_handle() -> AdapterHandle {
        let (handle, _event_rx, _join) = AdapterHandle::spawn(
            Box::new(TestBackend::new()),
            HashSet::new(),
        )
        .await
        .unwrap();
        handle
    }

    #[tokio::test]
    async fn test_script_get() {
        let handle = test_handle().await;
        let engine = ScriptEngine::new(handle);

        let result = engine.execute(r#"get("workspace")"#).await.unwrap();
        assert_eq!(result.as_int().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_script_set() {
        let handle = test_handle().await;
        let engine = ScriptEngine::new(handle);

        let script = r#"
            let current = get("layout");
            if current == "master" {
                let err = set("layout", "grid");
                if err != "" { throw err; }
            }
            get("layout")
        "#;

        let result = engine.execute(script).await.unwrap();
        assert_eq!(result.into_string().unwrap(), "grid");
    }

    #[tokio::test]
    async fn test_script_security() {
        let handle = test_handle().await;
        let engine = ScriptEngine::new(handle);

        // Trying to set a read-only value should return an error message
        let script = r#"set("monitor", "test")"#;
        let result = engine.execute(script).await.unwrap();
        let err_msg = result.into_string().unwrap();
        assert!(err_msg.contains("read-only"), "Expected read-only error, got: {}", err_msg);
    }

    #[tokio::test]
    async fn test_script_with_event_context() {
        let handle = test_handle().await;
        let engine = ScriptEngine::new(handle);

        let event = RawEvent::new("workspace_change")
            .with_prev("id", 1)
            .with_curr("id", 2)
            .with_curr("monitor", "HDMI-1");

        let script = r#"
            if curr.id > prev.id {
                "increased"
            } else {
                "decreased"
            }
        "#;

        let result = engine.execute_with_event(script, &event).await.unwrap();
        assert_eq!(result.into_string().unwrap(), "increased");
    }
}
