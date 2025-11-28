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
use middlesox::engine::ScriptEngine;
use middlesox::{available_backends, create_backend, BoxedAdapter, RawEvent};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
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
fn create_backend_from_config(adapter: &middlesox::config::AdapterConfig) -> Result<BoxedAdapter> {
    use middlesox::config::AdapterConfig;

    match adapter {
        AdapterConfig::Mock => create_backend("mock")
            .ok_or_else(|| anyhow!("Mock backend not available (enable 'mock' feature)")),
        AdapterConfig::Hyprland => {
            Err(anyhow!("Hyprland adapter not yet implemented"))
        }
        AdapterConfig::Mangowc => {
            Err(anyhow!("MangoWC adapter not yet implemented"))
        }
        AdapterConfig::Socket(socket_cfg) => {
            let (cmd_socket, event_socket) = socket_cfg.socket_paths();
            Ok(middlesox_socket::create_backend(cmd_socket, event_socket))
        }
    }
}

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Controller orchestrates the event pipeline and script execution.
struct Controller {
    config: Config,
    engine: Arc<ScriptEngine>,
    scripts_dir: PathBuf,
}

impl Controller {
    fn new(config: Config, engine: ScriptEngine, scripts_dir: PathBuf) -> Self {
        Self {
            config,
            engine: Arc::new(engine),
            scripts_dir,
        }
    }

    /// Process an incoming event.
    async fn handle_event(&self, event: RawEvent) {
        debug!(
            "Received event: {} prev={:?} curr={:?}",
            event.name, event.prev, event.curr
        );

        let prev = event.prev.clone().unwrap_or_default();
        let curr = event.curr.clone().unwrap_or_default();

        // Process declarative watches
        for watch in self.config.watches_for_event(&event.name) {
            if watch.matches(&prev, &curr) {
                info!("Watch matched: {} -> {}", event.name, watch.exec);
                self.run_script(&watch.exec, &event).await;
            }
        }

        // Process script watches
        for sw in self.config.script_watches_for_event(&event.name) {
            debug!("Running script watch: {} -> {}", event.name, sw.script);
            self.run_script(&sw.script, &event).await;
        }
    }

    async fn run_script(&self, script_name: &str, event: &RawEvent) {
        let script_path = self.scripts_dir.join(script_name);
        if !script_path.exists() {
            warn!("Script not found: {}", script_path.display());
            return;
        }

        match self.engine.execute_file_with_event(&script_path, event).await {
            Ok(result) => {
                debug!("Script '{}' returned: {:?}", script_name, result);
            }
            Err(e) => {
                error!("Script '{}' failed: {}", script_name, e);
            }
        }
    }

    /// Handle a control request from CLI.
    async fn handle_control(&self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::Get { key } => match self.engine.get(&key).await {
                Ok(value) => ControlResponse::ok(value),
                Err(e) => ControlResponse::err(e.to_string()),
            },

            ControlRequest::Set { key, value } => match self.engine.set(&key, value).await {
                Ok(()) => ControlResponse::ok_empty(),
                Err(e) => ControlResponse::err(e.to_string()),
            },

            ControlRequest::Exec { name } => {
                let cmd = match self.config.command_by_name(&name) {
                    Some(c) => c,
                    None => return ControlResponse::err(format!("Unknown command: {}", name)),
                };

                let script_path = self.scripts_dir.join(&cmd.script);
                if !script_path.exists() {
                    return ControlResponse::err(format!(
                        "Script not found: {}",
                        script_path.display()
                    ));
                }

                match self.engine.execute_file(&script_path).await {
                    Ok(result) => {
                        if result.is_unit() {
                            ControlResponse::ok_empty()
                        } else {
                            ControlResponse::ok(serde_json::json!(format!("{:?}", result)))
                        }
                    }
                    Err(e) => ControlResponse::err(e.to_string()),
                }
            }

            ControlRequest::Caps => {
                let manifest = self.engine.manifest();
                let caps: Vec<_> = manifest
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.name,
                            "access": format!("{:?}", c.access),
                            "description": c.description,
                        })
                    })
                    .collect();
                ControlResponse::ok(serde_json::json!(caps))
            }

            ControlRequest::Commands => {
                let cmds: Vec<_> = self
                    .config
                    .command
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.name,
                            "description": c.description,
                            "script": c.script,
                        })
                    })
                    .collect();
                ControlResponse::ok(serde_json::json!(cmds))
            }

            ControlRequest::Status => {
                let adapter_name = self.engine.adapter_name().await;
                ControlResponse::ok(serde_json::json!({
                    "running": true,
                    "pid": std::process::id(),
                    "adapter": adapter_name,
                }))
            }

            ControlRequest::Stop => {
                // This is handled specially in the main loop
                ControlResponse::ok_empty()
            }
        }
    }
}

/// Run the daemon (event watcher + control socket).
async fn run_daemon(config: Config) -> Result<()> {
    init_logging(&config.settings.log_level);

    // Acquire lock to prevent multiple instances
    let _lock = control::acquire_lock()?;
    info!("Lock acquired: {}", control::lock_path().display());

    info!("Middlesox starting...");
    info!("Available built-in backends: {:?}", available_backends());
    info!("Configured adapter: {}", config.adapter.name());

    let backend = create_backend_from_config(&config.adapter)?;
    info!("Adapter initialized: {}", backend.name());

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

    let scripts_dir = PathBuf::from(&config.settings.scripts_dir);
    if !scripts_dir.exists() {
        warn!("Scripts directory not found: {}", scripts_dir.display());
    }

    let engine = ScriptEngine::new(backend);
    let listener_backend = create_backend_from_config(&config.adapter)?;
    let subscriptions = config.subscribed_events();
    info!("Subscribed events: {:?}", subscriptions);

    let controller = Arc::new(Controller::new(config, engine, scripts_dir));

    // Create control socket
    let control_listener = control::create_listener().await?;
    info!("Control socket: {}", control::socket_path().display());

    // Channel for shutdown signal
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let shutdown_tx = Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));

    // Spawn event listener (backend -> daemon)
    let (event_tx, mut event_rx) = mpsc::channel::<RawEvent>(100);
    let listener_handle = tokio::spawn(async move {
        if let Err(e) = listener_backend.listen(event_tx, subscriptions).await {
            error!("Backend listener error: {}", e);
        }
    });

    // Spawn event processor
    let controller_for_events = controller.clone();
    let event_handle = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            controller_for_events.handle_event(event).await;
        }
        debug!("Event channel closed");
    });

    // Spawn control socket handler
    let controller_for_control = controller.clone();
    let shutdown_tx_for_control = shutdown_tx.clone();
    let control_handle = tokio::spawn(async move {
        handle_control_connections(control_listener, controller_for_control, shutdown_tx_for_control).await;
    });

    info!("Daemon running. Use 'msx stop' or Ctrl+C to stop.");

    // Wait for shutdown signal (Ctrl+C or stop command)
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received");
        }
        _ = &mut shutdown_rx => {
            info!("Stop command received");
        }
    }

    info!("Shutting down...");

    // Cleanup
    listener_handle.abort();
    event_handle.abort();
    control_handle.abort();
    control::cleanup_socket();

    info!("Middlesox stopped");
    Ok(())
}

/// Handle incoming control socket connections.
async fn handle_control_connections(
    listener: UnixListener,
    controller: Arc<Controller>,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let controller = controller.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_control_client(stream, controller, shutdown_tx).await {
                        debug!("Control client error: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("Control socket accept error: {}", e);
                break;
            }
        }
    }
}

/// Handle a single control client connection.
async fn handle_control_client(
    stream: tokio::net::UnixStream,
    controller: Arc<Controller>,
    shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    let request: ControlRequest = serde_json::from_str(line.trim())?;
    debug!("Control request: {:?}", request);

    // Check if this is a stop request
    let is_stop = matches!(request, ControlRequest::Stop);

    let response = controller.handle_control(request).await;

    let mut response_line = serde_json::to_string(&response)?;
    response_line.push('\n');
    writer.write_all(response_line.as_bytes()).await?;

    // If stop was requested, trigger shutdown
    if is_stop {
        if let Some(tx) = shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
    }

    Ok(())
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
            if let Some(path) = &config_path {
                println!("Loading config from: {}", path.display());
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
