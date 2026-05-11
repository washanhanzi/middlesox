//! Middlesox CLI
//!
//! Command-line interface for interacting with window manager backends.
//!
//! # Usage
//!
//! ```bash
//! # Start the daemon (required for other commands)
//! msx run
//!
//! # Check daemon status
//! msx status
//!
//! # Stop the daemon
//! msx stop
//!
//! # Query a value (talks to daemon)
//! msx get workspace
//! msx get layout
//!
//! # Set a value (talks to daemon)
//! msx set workspace 3
//! msx set layout grid
//!
//! # Execute a named command from config
//! msx exec cycle_layout
//!
//! # List available capabilities
//! msx caps
//!
//! # List available commands
//! msx commands
//! ```

mod control;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use control::{ControlRequest, ControlResponse};
use middlesox::config::Config;
use middlesox::controller::Controller;
use middlesox::{AdapterHandle, BoxedAdapter};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "msx")]
#[command(about = "Middlesox - Universal Window Manager Controller")]
#[command(version)]
struct Cli {
    /// Path to config file (default: auto-detect)
    #[arg(short, long, global = true)]
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

    /// Execute a named command from config
    Exec {
        /// Command name (as defined in [[command]] sections)
        name: String,
    },

    /// List available capabilities
    Caps,

    /// List available commands from config
    Commands,

    /// Start the daemon (listens for events, handles commands)
    Run,

    /// Stop the running daemon
    Stop,

    /// Check if daemon is running
    Status,
}

fn find_config_path() -> Option<PathBuf> {
    let candidates = [
        // Local directory config (highest priority)
        PathBuf::from("./middlesox.toml"),
        // User config
        dirs::config_dir()
            .map(|p| p.join("middlesox/config.toml"))
            .unwrap_or_default(),
        // System config
        PathBuf::from("/etc/middlesox/config.toml"),
    ];

    candidates.into_iter().find(|p| p.exists())
}

/// Expand path with tilde expansion.
fn expand_path(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
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
fn create_backend_from_config(adapter: &middlesox::config::AdapterConfig) -> Result<BoxedAdapter> {
    match adapter.name.as_str() {
        "mock" => Ok(middlesox_mock::create_backend()),
        "mangowc" => middlesox_mangowc::create_backend(),
        "hyprland" => Err(anyhow!(
            "Hyprland adapter is not yet implemented. \
             Use the socket adapter with an external Hyprland bridge instead."
        )),
        "socket" => {
            let (cmd_socket, event_socket) = socket_paths_from_options(&adapter.options)?;
            Ok(middlesox_socket::create_backend(cmd_socket, event_socket))
        }
        other => Err(anyhow!("Unknown adapter: '{}'", other)),
    }
}

/// Extract socket paths from adapter options.
///
/// Supports two modes:
/// - Explicit: `cmd_socket` + `event_socket`
/// - Base path: `base_path` derives `{base}.sock` and `{base}-events.sock`
fn socket_paths_from_options(
    options: &std::collections::HashMap<String, toml::Value>,
) -> Result<(String, String)> {
    let get_str = |key: &str| -> Option<&str> {
        options.get(key).and_then(|v| v.as_str())
    };

    if let (Some(cmd), Some(event)) = (get_str("cmd_socket"), get_str("event_socket")) {
        return Ok((cmd.to_string(), event.to_string()));
    }

    if let Some(base) = get_str("base_path") {
        return Ok((format!("{base}.sock"), format!("{base}-events.sock")));
    }

    Err(anyhow!(
        "Socket adapter requires 'cmd_socket'+'event_socket' or 'base_path' in [adapter]"
    ))
}

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Run the daemon (event watcher + control socket).
async fn run_daemon(config: Config) -> Result<()> {
    // Acquire lock to prevent multiple instances
    let _lock = control::acquire_lock()?;
    info!("Lock acquired: {}", control::lock_path().display());

    info!("Middlesox starting...");
    info!("Configured adapter: {}", config.adapter.name);

    let backend = create_backend_from_config(&config.adapter)?;
    info!("Adapter created: {}", backend.name());

    // Show capabilities
    let manifest = backend.manifest();
    info!("Capabilities ({}):", manifest.len());
    for cap in manifest.iter() {
        info!(
            "  - {} ({:?}): {}",
            cap.name,
            cap.access,
            cap.description.as_deref().unwrap_or("no description")
        );
    }

    let scripts_dir = expand_path(&config.settings.scripts_dir);
    if !scripts_dir.exists() {
        warn!("Scripts directory not found: {}", scripts_dir.display());
    }

    let subscriptions = config.subscribed_events();
    info!("Subscribed events: {:?}", subscriptions);

    // Spawn the adapter actor — returns handle + event receiver
    let (adapter_handle, event_rx, _adapter_task) =
        AdapterHandle::spawn(backend, subscriptions).await?;

    info!("Adapter initialized: {}", adapter_handle.name());

    // Create controller (owns adapter handle + script engine)
    let controller = Controller::new(config, adapter_handle, scripts_dir);

    // Create control socket
    let control_listener = control::create_listener().await?;
    info!("Control socket: {}", control::socket_path().display());

    // Shutdown signal (Ctrl+C)
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_tx = Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));

    // Wire Ctrl+C to shutdown
    let shutdown_tx_for_ctrlc = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Ctrl+C received");
            if let Some(tx) = shutdown_tx_for_ctrlc.lock().await.take() {
                let _ = tx.send(());
            }
        }
    });

    // Run the main event loop (blocks until shutdown)
    let run_result = controller.run(event_rx, control_listener, shutdown_rx).await;

    // Cleanup
    control::cleanup_socket();

    run_result
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config
    let config_path = cli.config.or_else(find_config_path);
    let config = match &config_path {
        Some(path) => Config::load(path)?,
        None => Config::default_config(),
    };

    match cli.command {
        Commands::Run => {
            init_logging(&config.settings.log_level);
            if let Some(path) = &config_path {
                info!("Loading config from: {}", path.display());
            }
            run_daemon(config).await
        }

        Commands::Get { key } => {
            let response = control::send_request(&ControlRequest::Get { key }).await?;
            handle_response(response, |v| {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            })
        }

        Commands::Set { key, value } => {
            let response = control::send_request(&ControlRequest::Set {
                key,
                value: parse_value(&value),
            })
            .await?;
            handle_response(response, |_| println!("OK"))
        }

        Commands::Exec { name } => {
            let response = control::send_request(&ControlRequest::Exec { name }).await?;
            handle_response(response, |v| {
                if !v.is_null() {
                    println!("{}", v);
                }
            })
        }

        Commands::Caps => {
            let response = control::send_request(&ControlRequest::Caps).await?;
            handle_response(response, |v| {
                if let Some(caps) = v.as_array() {
                    println!("Capabilities ({}):", caps.len());
                    for cap in caps {
                        println!(
                            "  {} ({}): {}",
                            cap["name"].as_str().unwrap_or("-"),
                            cap["access"].as_str().unwrap_or("-"),
                            cap["description"].as_str().unwrap_or("-")
                        );
                    }
                }
            })
        }

        Commands::Commands => {
            let response = control::send_request(&ControlRequest::Commands).await?;
            handle_response(response, |v| {
                if let Some(cmds) = v.as_array() {
                    if cmds.is_empty() {
                        println!("No commands defined in config");
                    } else {
                        println!("Available commands:");
                        for cmd in cmds {
                            println!(
                                "  {}: {}",
                                cmd["name"].as_str().unwrap_or("-"),
                                cmd["description"]
                                    .as_str()
                                    .or_else(|| cmd["script"].as_str())
                                    .unwrap_or("-")
                            );
                        }
                    }
                }
            })
        }

        Commands::Stop => {
            let response = control::send_request(&ControlRequest::Stop).await?;
            handle_response(response, |_| println!("Daemon stopped"))
        }

        Commands::Status => {
            if !control::is_daemon_running() {
                println!("Daemon is not running");
                return Ok(());
            }

            let response = control::send_request(&ControlRequest::Status).await?;
            handle_response(response, |v| {
                println!("Status: running");
                if let Some(pid) = v["pid"].as_u64() {
                    println!("PID: {}", pid);
                }
                if let Some(adapter) = v["adapter"].as_str() {
                    println!("Adapter: {}", adapter);
                }
            })
        }
    }
}

/// Handle a control response, printing errors or calling the success handler.
fn handle_response(response: ControlResponse, on_success: impl FnOnce(Value)) -> Result<()> {
    if response.success {
        on_success(response.result.unwrap_or(Value::Null));
        Ok(())
    } else {
        Err(anyhow!(
            "{}",
            response.error.unwrap_or_else(|| "Unknown error".into())
        ))
    }
}
