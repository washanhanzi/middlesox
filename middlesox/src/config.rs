//! Configuration loader for Middlesox.
//!
//! Parses the TOML configuration file that defines rules for
//! event-triggered script execution.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::debug;

/// Root configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Global settings
    #[serde(default)]
    pub settings: Settings,

    /// Adapter configuration
    #[serde(default)]
    pub adapter: AdapterConfig,

    /// Event watches: match event + optional prev/curr conditions, execute script
    /// If prev/curr are empty, script runs for every matching event (script handles logic).
    /// If prev/curr have conditions, declarative matching is applied first.
    #[serde(default)]
    pub watch: Vec<Watch>,

    /// Named reusable commands
    #[serde(default)]
    pub command: Vec<Command>,
}

/// Global settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Directory containing scripts.
    ///
    /// Absolute paths and `~` paths are used as-is.
    /// Relative paths are resolved relative to the config file's directory.
    #[serde(default = "default_scripts_dir")]
    pub scripts_dir: String,

    /// Log level ("trace", "debug", "info", "warn", "error")
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Environment variables passed to executable scripts.
    #[serde(default)]
    pub script_env: HashMap<String, String>,
}

/// Adapter configuration.
///
/// The core is adapter-agnostic — it only stores the adapter name and
/// any adapter-specific options as a freeform table. The CLI binary
/// interprets the name and options to construct the concrete adapter.
///
/// # Examples
///
/// ```toml
/// [adapter]
/// name = "mock"
///
/// [adapter]
/// name = "hyprland"
///
/// [adapter]
/// name = "socket"
/// cmd_socket = "/run/user/1000/my-bridge.sock"
/// event_socket = "/run/user/1000/my-bridge-events.sock"
///
/// [adapter]
/// name = "socket"
/// base_path = "/run/user/1000/my-bridge"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterConfig {
    /// Adapter name (e.g., "mock", "mangowc", "hyprland", "socket").
    #[serde(default = "default_adapter_name")]
    pub name: String,

    /// Adapter-specific options (freeform key-value pairs).
    /// Each adapter interprets these differently.
    #[serde(flatten)]
    pub options: HashMap<String, toml::Value>,
}

fn default_adapter_name() -> String {
    "mock".into()
}

impl Default for AdapterConfig {
    fn default() -> Self {
        Self {
            name: default_adapter_name(),
            options: HashMap::new(),
        }
    }
}

fn default_scripts_dir() -> String {
    dirs::config_dir()
        .map(|p| p.join("middlesox/scripts"))
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config/middlesox/scripts"))
        .to_string_lossy()
        .into_owned()
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scripts_dir: default_scripts_dir(),
            log_level: default_log_level(),
            script_env: HashMap::new(),
        }
    }
}

/// A simple declarative watch.
///
/// Matches events based on prev/curr state and executes a script.
/// The script receives event context (prev, curr, event_name) but
/// matching is done by the config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watch {
    /// Event name to match (e.g., "workspace_change")
    pub event: String,

    /// Script file to execute (relative to scripts_dir)
    pub exec: String,

    /// Optional: match only if previous state matches these values
    #[serde(default)]
    pub prev: HashMap<String, Value>,

    /// Optional: match only if current state matches these values
    #[serde(default)]
    pub curr: HashMap<String, Value>,

    /// Optional: human-readable description
    pub description: Option<String>,

    /// Whether the watch is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// A named reusable command.
///
/// Commands can be invoked by name from other scripts or externally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    /// Command name (e.g., "cycle_layout")
    pub name: String,

    /// Script file to execute (relative to scripts_dir)
    pub script: String,

    /// Optional: human-readable description
    pub description: Option<String>,
}

fn default_enabled() -> bool {
    true
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// Relative `scripts_dir` paths are resolved relative to the config file's
    /// parent directory.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config = Self::parse(&content)?;

        // Resolve relative scripts_dir against the config file's parent directory
        let scripts_path = std::path::Path::new(&config.settings.scripts_dir);
        if !scripts_path.is_absolute() && !config.settings.scripts_dir.starts_with('~') {
            if let Some(config_dir) = path.parent() {
                config.settings.scripts_dir = config_dir
                    .join(scripts_path)
                    .to_string_lossy()
                    .into_owned();
            }
        }

        Ok(config)
    }

    /// Parse configuration from a TOML string.
    pub fn parse(content: &str) -> Result<Self> {
        toml::from_str(content).context("Failed to parse TOML config")
    }

    /// Get enabled watches for a specific event.
    pub fn watches_for_event(&self, event_name: &str) -> Vec<&Watch> {
        self.watch
            .iter()
            .filter(|w| w.enabled && w.event == event_name)
            .collect()
    }

    /// Get a command by name.
    pub fn command_by_name(&self, name: &str) -> Option<&Command> {
        self.command.iter().find(|c| c.name == name)
    }

    /// Get all unique event names that have enabled watches.
    ///
    /// This is used to tell backends which events to subscribe to.
    pub fn subscribed_events(&self) -> HashSet<String> {
        self.watch
            .iter()
            .filter(|w| w.enabled)
            .map(|w| w.event.clone())
            .collect()
    }

    /// Create a default configuration.
    pub fn default_config() -> Self {
        Self {
            settings: Settings::default(),
            adapter: AdapterConfig::default(), // "mock"
            watch: vec![
                Watch {
                    event: "workspace_change".into(),
                    exec: "on_workspace_change.rhai".into(),
                    prev: HashMap::new(),
                    curr: HashMap::new(),
                    description: Some("Called when workspace changes".into()),
                    enabled: true,
                },
            ],
            command: vec![],
        }
    }
}

impl Watch {
    /// Check if this watch matches the given transition states.
    pub fn matches(&self, prev_state: &HashMap<String, Value>, curr_state: &HashMap<String, Value>) -> bool {
        debug!(
            event = %self.event,
            exec = %self.exec,
            prev_conditions = ?self.prev,
            curr_conditions = ?self.curr,
            prev_state = ?prev_state,
            curr_state = ?curr_state,
            "Checking watch match"
        );

        // Check all prev conditions
        for (key, expected) in &self.prev {
            match prev_state.get(key) {
                Some(actual) if actual == expected => {
                    debug!(key = %key, expected = ?expected, "prev condition matched");
                    continue;
                }
                Some(actual) => {
                    debug!(key = %key, expected = ?expected, actual = ?actual, "prev condition failed: value mismatch");
                    return false;
                }
                None => {
                    debug!(key = %key, expected = ?expected, "prev condition failed: key not found");
                    return false;
                }
            }
        }

        // Check all curr conditions
        for (key, expected) in &self.curr {
            match curr_state.get(key) {
                Some(actual) if actual == expected => {
                    debug!(key = %key, expected = ?expected, "curr condition matched");
                    continue;
                }
                Some(actual) => {
                    debug!(key = %key, expected = ?expected, actual = ?actual, "curr condition failed: value mismatch");
                    return false;
                }
                None => {
                    debug!(key = %key, expected = ?expected, "curr condition failed: key not found");
                    return false;
                }
            }
        }

        debug!(event = %self.event, exec = %self.exec, "Watch matched");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let toml = r#"
[settings]
scripts_dir = "scripts"

[settings.script_env]
MSX_WALLPAPER_DIR = "~/Pictures/wallpapers"

[adapter]
name = "mock"

[[watch]]
event = "workspace_change"
exec = "wallpaper.rhai"
description = "Change wallpaper on workspace switch"

[watch.prev]
id = 1

[watch.curr]
id = 2

# Script watch (no conditions)
[[watch]]
event = "layout_change"
exec = "smart_layout.rhai"

[[command]]
name = "toggle_layout"
script = "toggle_layout.rhai"
"#;

        let config = Config::parse(toml).unwrap();
        assert_eq!(
            config.settings.script_env.get("MSX_WALLPAPER_DIR").map(String::as_str),
            Some("~/Pictures/wallpapers")
        );
        assert_eq!(config.adapter.name, "mock");
        assert_eq!(config.watch.len(), 2);
        assert_eq!(config.watch[0].event, "workspace_change");
        assert_eq!(config.watch[0].prev.get("id"), Some(&Value::from(1)));
        assert_eq!(config.watch[1].event, "layout_change");
        assert!(config.watch[1].prev.is_empty()); // No conditions = script watch
        assert_eq!(config.command.len(), 1);
        assert_eq!(config.command[0].name, "toggle_layout");
    }

    #[test]
    fn test_parse_socket_adapter_explicit() {
        let toml = r#"
[adapter]
name = "socket"
cmd_socket = "/run/user/1000/bridge.sock"
event_socket = "/run/user/1000/bridge-events.sock"
"#;

        let config = Config::parse(toml).unwrap();
        assert_eq!(config.adapter.name, "socket");
        assert_eq!(
            config.adapter.options.get("cmd_socket").and_then(|v| v.as_str()),
            Some("/run/user/1000/bridge.sock")
        );
        assert_eq!(
            config.adapter.options.get("event_socket").and_then(|v| v.as_str()),
            Some("/run/user/1000/bridge-events.sock")
        );
    }

    #[test]
    fn test_parse_socket_adapter_base_path() {
        let toml = r#"
[adapter]
name = "socket"
base_path = "/run/user/1000/bridge"
"#;

        let config = Config::parse(toml).unwrap();
        assert_eq!(config.adapter.name, "socket");
        assert_eq!(
            config.adapter.options.get("base_path").and_then(|v| v.as_str()),
            Some("/run/user/1000/bridge")
        );
    }

    #[test]
    fn test_parse_hyprland_adapter() {
        let toml = r#"
[adapter]
name = "hyprland"
"#;

        let config = Config::parse(toml).unwrap();
        assert_eq!(config.adapter.name, "hyprland");
        assert!(config.adapter.options.is_empty());
    }

    #[test]
    fn test_watch_matching() {
        let watch = Watch {
            event: "workspace_change".into(),
            exec: "test.rhai".into(),
            prev: [("id".into(), Value::from(1))].into_iter().collect(),
            curr: [("id".into(), Value::from(2))].into_iter().collect(),
            description: None,
            enabled: true,
        };

        let prev = [("id".into(), Value::from(1))].into_iter().collect();
        let curr = [("id".into(), Value::from(2))].into_iter().collect();
        assert!(watch.matches(&prev, &curr));

        // Wrong prev
        let prev = [("id".into(), Value::from(3))].into_iter().collect();
        assert!(!watch.matches(&prev, &curr));
    }

    #[test]
    fn test_subscribed_events() {
        let toml = r#"
[[watch]]
event = "workspace_change"
exec = "test.rhai"

[[watch]]
event = "focus_change"
exec = "focus.rhai"

[[watch]]
event = "workspace_change"
exec = "ws.rhai"
"#;

        let config = Config::parse(toml).unwrap();
        let events = config.subscribed_events();
        assert_eq!(events.len(), 2);
        assert!(events.contains("workspace_change"));
        assert!(events.contains("focus_change"));
    }
}
