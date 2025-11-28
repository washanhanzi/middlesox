//! Middlesox CLI
//!
//! Command-line interface for interacting with window manager backends.
//!
//! # Usage
//!
//! ```bash
//! # Query a value
//! msx get workspace
//! msx get layout
//!
//! # Set a value
//! msx set workspace 3
//! msx set layout grid
//!
//! # Run a named command
//! msx run cycle_layout
//!
//! # List available capabilities
//! msx caps
//!
//! # List available commands
//! msx commands
//! ```

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use middlesox::config::Config;
use middlesox::engine::ScriptEngine;
use middlesox::{create_backend, BoxedAdapter};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "msx")]
#[command(about = "Middlesox CLI - interact with window manager backends")]
#[command(version)]
struct Cli {
    /// Path to config file (default: auto-detect)
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Get a value from the backend
    Get {
        /// Key to query (e.g., "workspace", "layout")
        key: String,
    },

    /// Set a value in the backend
    Set {
        /// Key to set (e.g., "workspace", "layout")
        key: String,

        /// Value to set (interpreted as JSON if possible, otherwise string)
        value: String,
    },

    /// Run a named command from config
    Run {
        /// Command name (as defined in [[command]] sections)
        name: String,
    },

    /// List available capabilities
    Caps,

    /// List available commands from config
    Commands,
}

fn find_config_path() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("middlesox.toml"),
        dirs::config_dir()
            .map(|p| p.join("middlesox/config.toml"))
            .unwrap_or_default(),
        PathBuf::from("/etc/middlesox/config.toml"),
    ];

    candidates.into_iter().find(|p| p.exists())
}

fn parse_value(s: &str) -> Value {
    // Try parsing as JSON first
    if let Ok(v) = serde_json::from_str(s) {
        return v;
    }

    // Try as integer
    if let Ok(i) = s.parse::<i64>() {
        return Value::from(i);
    }

    // Try as float
    if let Ok(f) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }

    // Try as boolean
    match s.to_lowercase().as_str() {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }

    // Default to string
    Value::String(s.to_string())
}

/// Create a backend from adapter config.
///
/// Supports built-in backends and external socket adapter.
fn create_backend_from_config(adapter: &middlesox::config::AdapterConfig) -> Result<BoxedAdapter> {
    use middlesox::config::AdapterConfig;

    match adapter {
        AdapterConfig::Mock => {
            create_backend("mock")
                .ok_or_else(|| anyhow!("Mock backend not available (enable 'mock' feature)"))
        }
        AdapterConfig::Hyprland => {
            // TODO: Use middlesox_hyprland::create_backend() when implemented
            Err(anyhow!("Hyprland adapter not yet implemented"))
        }
        AdapterConfig::Mangowc => {
            // TODO: Use middlesox_mangowc::create_backend() when implemented
            Err(anyhow!("MangoWC adapter not yet implemented"))
        }
        AdapterConfig::Socket(socket_cfg) => {
            let (cmd_socket, event_socket) = socket_cfg.socket_paths();
            Ok(middlesox_socket::create_backend(cmd_socket, event_socket))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config
    let config_path = cli.config.or_else(find_config_path);
    let config = match config_path {
        Some(path) => Config::load(&path)?,
        None => Config::default_config(),
    };

    // Create backend from adapter config
    let backend = create_backend_from_config(&config.adapter)?;

    match cli.command {
        Commands::Get { key } => {
            let value = backend.get(&key).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }

        Commands::Set { key, value } => {
            let parsed = parse_value(&value);
            backend.set(&key, parsed).await?;
            println!("OK");
        }

        Commands::Run { name } => {
            let cmd = config
                .command_by_name(&name)
                .ok_or_else(|| anyhow!("Unknown command: {}", name))?;

            let scripts_dir = PathBuf::from(&config.settings.scripts_dir);
            let script_path = scripts_dir.join(&cmd.script);

            if !script_path.exists() {
                return Err(anyhow!("Script not found: {}", script_path.display()));
            }

            let engine = ScriptEngine::new(backend);
            let result = engine.execute_file(&script_path).await?;

            // Print result if not unit
            if !result.is_unit() {
                println!("{:?}", result);
            }
        }

        Commands::Caps => {
            let manifest = backend.manifest();
            println!("Capabilities ({}):", manifest.len());
            for cap in manifest.iter() {
                println!(
                    "  {} ({:?}): {}",
                    cap.name,
                    cap.access,
                    cap.description.as_deref().unwrap_or("-")
                );
            }
        }

        Commands::Commands => {
            if config.command.is_empty() {
                println!("No commands defined in config");
            } else {
                println!("Available commands:");
                for cmd in &config.command {
                    println!(
                        "  {}: {}",
                        cmd.name,
                        cmd.description.as_deref().unwrap_or(&cmd.script)
                    );
                }
            }
        }
    }

    Ok(())
}
