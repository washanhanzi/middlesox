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
    use middlesox::config::AdapterConfig;

    match adapter {
        AdapterConfig::Mock => create_backend("mock")
            .ok_or_else(|| anyhow!("Mock backend not available (enable 'mock' feature)")),
        AdapterConfig::Hyprland => {
            Err(anyhow!("Hyprland adapter not yet implemented"))
        }
        AdapterConfig::Mangowc => {
            middlesox_mangowc::create_backend()
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

        // Process watches
        for watch in self.config.watches_for_event(&event.name) {
            // If no conditions specified, always run (script handles logic)
            // If conditions specified, check match first
            let should_run = watch.prev.is_empty() && watch.curr.is_empty()
                || watch.matches(&prev, &curr);

            if should_run {
                info!("Watch triggered: {} -> {}", event.name, watch.exec);
                self.run_script(&watch.exec, &event).await;
            }
        }
    }

    async fn run_script(&self, script_name: &str, event: &RawEvent) {
        let script_path = self.resolve_script_path(script_name);
        if !script_path.exists() {
            warn!("Script not found: {}", script_path.display());
            return;
        }

        // Check if it's a Rhai script or a shell command
        if script_path.extension().map_or(false, |ext| ext == "rhai") {
            // Execute as Rhai script
            match self.engine.execute_file_with_event(&script_path, event).await {
                Ok(result) => {
                    debug!("Script '{}' returned: {:?}", script_name, result);
                }
                Err(e) => {
                    error!("Script '{}' failed: {}", script_name, e);
                }
            }
        } else {
            // Execute as shell command
            self.run_shell_script(&script_path, script_name, Some(event)).await;
        }
    }

    /// Resolve a script path, supporting both absolute paths and relative paths.
    fn resolve_script_path(&self, script_name: &str) -> PathBuf {
        if std::path::Path::new(script_name).is_absolute() {
            PathBuf::from(script_name)
        } else {
            self.scripts_dir.join(script_name)
        }
    }

    /// Run a shell script with optional event context.
    async fn run_shell_script(&self, script_path: &PathBuf, script_name: &str, event: Option<&RawEvent>) {
        debug!("Spawning command: {}", script_path.display());

        // Validate shebang for .sh files
        if script_path.extension().map_or(false, |ext| ext == "sh") {
            match std::fs::read(script_path) {
                Ok(content) => {
                    if !content.starts_with(b"#!") {
                        error!(
                            "Script '{}' is missing a shebang line (e.g., #!/bin/bash). \
                             This will cause 'Exec format error' on execution.",
                            script_path.display()
                        );
                        return;
                    }
                }
                Err(e) => {
                    error!("Failed to read script '{}': {}", script_path.display(), e);
                    return;
                }
            }
        }

        let script_name = script_name.to_string();
        let script_path = script_path.clone();

        // Build environment variables and JSON input
        let (input_json, env_event, env_prev, env_curr) = if let Some(event) = event {
            let input_json = serde_json::json!({
                "event": &event.name,
                "prev": &event.prev,
                "curr": &event.curr,
            })
            .to_string();
            let env_event = event.name.clone();
            let env_prev = serde_json::to_string(&event.prev).unwrap_or_default();
            let env_curr = serde_json::to_string(&event.curr).unwrap_or_default();
            (input_json, env_event, env_prev, env_curr)
        } else {
            // No event context - command invocation
            let input_json = serde_json::json!({
                "event": "command",
                "prev": null,
                "curr": null,
            })
            .to_string();
            ("command".to_string(), "null".to_string(), "null".to_string());
            (input_json, "command".to_string(), "null".to_string(), "null".to_string())
        };

        tokio::spawn(async move {
            let result = tokio::process::Command::new(&script_path)
                .arg(&input_json)
                // Set environment variables for shell scripts
                .env("MSX_EVENT", &env_event)
                .env("MSX_PREV", &env_prev)
                .env("MSX_CURR", &env_curr)
                .output()
                .await;

            match result {
                Ok(output) => {
                    if output.status.success() {
                        debug!("Command '{}' completed successfully", script_name);
                        if !output.stdout.is_empty() {
                            debug!("stdout: {}", String::from_utf8_lossy(&output.stdout));
                        }
                    } else {
                        error!(
                            "Command '{}' failed with status: {}",
                            script_name, output.status
                        );
                        if !output.stderr.is_empty() {
                            error!("stderr: {}", String::from_utf8_lossy(&output.stderr));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to execute command '{}': {}", script_name, e);
                }
            }
        });
    }

    /// Execute a script synchronously and return the result.
    /// Used by msx exec command.
    async fn execute_script(&self, script_name: &str) -> Result<Option<String>, String> {
        let script_path = self.resolve_script_path(script_name);
        if !script_path.exists() {
            return Err(format!("Script not found: {}", script_path.display()));
        }

        // Check if it's a Rhai script or a shell command
        if script_path.extension().map_or(false, |ext| ext == "rhai") {
            // Execute as Rhai script
            match self.engine.execute_file(&script_path).await {
                Ok(result) => {
                    if result.is_unit() {
                        Ok(None)
                    } else {
                        Ok(Some(format!("{:?}", result)))
                    }
                }
                Err(e) => Err(e.to_string()),
            }
        } else {
            // Execute as shell command synchronously
            self.execute_shell_script(&script_path, script_name).await
        }
    }

    /// Execute a shell script synchronously and return the result.
    async fn execute_shell_script(&self, script_path: &PathBuf, script_name: &str) -> Result<Option<String>, String> {
        debug!("Executing command: {}", script_path.display());

        // Validate shebang for .sh files
        if script_path.extension().map_or(false, |ext| ext == "sh") {
            match std::fs::read(script_path) {
                Ok(content) => {
                    if !content.starts_with(b"#!") {
                        return Err(format!(
                            "Script '{}' is missing a shebang line (e.g., #!/bin/bash)",
                            script_path.display()
                        ));
                    }
                }
                Err(e) => {
                    return Err(format!("Failed to read script '{}': {}", script_path.display(), e));
                }
            }
        }

        // No event context for command invocation
        let input_json = serde_json::json!({
            "event": "command",
            "prev": null,
            "curr": null,
        })
        .to_string();

        let result = tokio::process::Command::new(script_path)
            .arg(&input_json)
            .env("MSX_EVENT", "command")
            .env("MSX_PREV", "null")
            .env("MSX_CURR", "null")
            .output()
            .await;

        match result {
            Ok(output) => {
                if output.status.success() {
                    debug!("Command '{}' completed successfully", script_name);
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    if stdout.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(stdout))
                    }
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    Err(format!(
                        "Command '{}' failed with status {}: {}",
                        script_name, output.status, stderr
                    ))
                }
            }
            Err(e) => Err(format!("Failed to execute command '{}': {}", script_name, e)),
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

                // Use execute_script which supports both Rhai and shell scripts
                match self.execute_script(&cmd.script).await {
                    Ok(None) => ControlResponse::ok_empty(),
                    Ok(Some(output)) => ControlResponse::ok(serde_json::json!(output)),
                    Err(e) => ControlResponse::err(e),
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

    // For socket adapters, trigger capability fetch before creating engine
    // by doing a dummy get (capabilities are lazily fetched on first operation)
    if matches!(config.adapter, middlesox::config::AdapterConfig::Socket(_)) {
        debug!("Triggering capability fetch for socket adapter...");
        // Ignore error - this just primes the capability cache
        let _ = backend.get("__caps_prime__").await;
    }

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

    // Create engine with the backend - it will be shared for event listening
    let engine = ScriptEngine::new(backend);

    // Get the shared adapter for the event listener (same instance as engine uses)
    let shared_adapter = engine.shared_adapter();

    let subscriptions = config.subscribed_events();
    info!("Subscribed events: {:?}", subscriptions);

    let controller = Arc::new(Controller::new(config, engine, scripts_dir));

    // Create control socket
    let control_listener = control::create_listener().await?;
    info!("Control socket: {}", control::socket_path().display());

    // Channel for shutdown signal
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let shutdown_tx = Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));

    // Spawn event listener (backend -> daemon) using the shared adapter
    let (event_tx, mut event_rx) = mpsc::channel::<RawEvent>(100);
    let listener_adapter = shared_adapter.clone();
    let listener_adapter_for_shutdown = shared_adapter.clone();
    let listener_handle = tokio::spawn(async move {
        let adapter = listener_adapter.read().await;
        if let Err(e) = adapter.listen(event_tx, subscriptions).await {
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

    // Signal the backend to stop its event loop
    debug!("Calling backend shutdown()...");
    {
        let adapter = listener_adapter_for_shutdown.read().await;
        if let Err(e) = adapter.shutdown().await {
            warn!("Backend shutdown error: {}", e);
        }
    }
    debug!("Backend shutdown() returned");

    // Give the listener task a moment to exit cleanly
    debug!("Waiting for listener task to exit...");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    debug!("Wait complete, aborting tasks...");

    // Cleanup
    listener_handle.abort();
    debug!("listener_handle aborted");
    event_handle.abort();
    debug!("event_handle aborted");
    control_handle.abort();
    debug!("control_handle aborted");
    control::cleanup_socket();
    debug!("Socket cleaned up");

    info!("Middlesox stopped");
    debug!("Returning from run_daemon()");
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
