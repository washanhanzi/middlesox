//! Rhai scripting engine with security enforcement.
//!
//! This module wraps the Rhai scripting engine and provides secure
//! access to window manager state through the protocol adapter.

use crate::{BoxedAdapter, CapabilityManifest, RawEvent};
use anyhow::{anyhow, Result};
use rhai::{Dynamic, Engine, Scope};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
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
/// manifest before allowing them to execute.
pub struct ScriptEngine {
    adapter: Arc<RwLock<BoxedAdapter>>,
    manifest: CapabilityManifest,
}

impl ScriptEngine {
    /// Create a new script engine with the given adapter.
    pub fn new(adapter: BoxedAdapter) -> Self {
        let manifest = adapter.manifest();

        Self {
            adapter: Arc::new(RwLock::new(adapter)),
            manifest,
        }
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
    /// - `set("key", value)` - Set a value in the window manager (security-checked)
    pub async fn execute(&self, script: &str) -> Result<Dynamic> {
        self.execute_with_context(script, None).await
    }

    /// Execute a script with event context.
    ///
    /// The script can use:
    /// - `get("key")` - Query a value from the window manager
    /// - `set("key", value)` - Set a value in the window manager (security-checked)
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
        let manifest = self.manifest.clone();
        let script = script.to_string();

        // Run the script in a blocking task since Rhai isn't async
        let result = tokio::task::spawn_blocking(move || {
            Self::execute_sync(&script, adapter, manifest, context)
        })
        .await
        .map_err(|e| anyhow!("Script task panicked: {}", e))??;

        Ok(result)
    }

    fn execute_sync(
        script: &str,
        adapter: Arc<RwLock<BoxedAdapter>>,
        manifest: CapabilityManifest,
        context: Option<EventContext>,
    ) -> Result<Dynamic> {
        // Create a new runtime for blocking operations
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
        let manifest_set = manifest.clone();

        // Register the `get` function
        engine.register_fn("get", move |key: &str| -> Dynamic {
            let adapter = adapter_get.clone();
            let key = key.to_string();

            rt.block_on(async {
                let adapter = adapter.read().await;
                match adapter.get(&key).await {
                    Ok(value) => json_to_dynamic(value),
                    Err(e) => {
                        error!("Script get('{}') failed: {}", key, e);
                        Dynamic::UNIT
                    }
                }
            })
        });

        // Register the `set` function with security enforcement
        let rt2 = tokio::runtime::Handle::current();
        engine.register_fn("set", move |key: &str, value: Dynamic| -> bool {
            let adapter = adapter_set.clone();
            let manifest = manifest_set.clone();
            let key = key.to_string();

            // Security check: validate against manifest
            let capability = match manifest.find(&key) {
                Some(cap) => cap.clone(),
                None => {
                    warn!("Script tried to set unknown key: {}", key);
                    return false;
                }
            };

            if !capability.is_writable() {
                warn!(
                    "Security violation: script tried to write read-only key '{}'",
                    key
                );
                return false;
            }

            let json_value = dynamic_to_json(value);

            rt2.block_on(async {
                let adapter = adapter.read().await;
                match adapter.set(&key, json_value).await {
                    Ok(()) => {
                        debug!("Script set('{}') succeeded", key);
                        true
                    }
                    Err(e) => {
                        error!("Script set('{}') failed: {}", key, e);
                        false
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

    /// Get the capability manifest.
    pub fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
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

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::MockBackend;

    #[tokio::test]
    async fn test_script_get() {
        let backend = MockBackend::new();
        let engine = ScriptEngine::new(Box::new(backend));

        let result = engine.execute(r#"get("workspace")"#).await.unwrap();
        assert_eq!(result.as_int().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_script_set() {
        let backend = MockBackend::new();
        let engine = ScriptEngine::new(Box::new(backend));

        let script = r#"
            let current = get("layout");
            if current == "master" {
                set("layout", "grid");
            }
            get("layout")
        "#;

        let result = engine.execute(script).await.unwrap();
        assert_eq!(result.into_string().unwrap(), "grid");
    }

    #[tokio::test]
    async fn test_script_security() {
        let backend = MockBackend::new();
        let engine = ScriptEngine::new(Box::new(backend));

        // Trying to set a read-only value should fail (return false)
        let script = r#"set("monitor", "test")"#;
        let result = engine.execute(script).await.unwrap();
        assert!(!result.as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_script_with_event_context() {
        use crate::RawEvent;

        let backend = MockBackend::new();
        let engine = ScriptEngine::new(Box::new(backend));

        let event = RawEvent::new("workspace_change")
            .with_prev("id", 1)
            .with_curr("id", 2)
            .with_curr("monitor", "HDMI-1");

        // Script can access event context
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
