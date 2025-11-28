//! Middlesox Daemon - Universal Window Manager Controller
//!
//! A scriptable event-driven controller for window managers and compositors.

use anyhow::{anyhow, Result};
use middlesox::config::Config;
use middlesox::engine::ScriptEngine;
use middlesox::{available_backends, create_backend, BoxedAdapter, RawEvent};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

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

        // Use event's prev/curr directly for watch matching
        let prev = event.prev.clone().unwrap_or_default();
        let curr = event.curr.clone().unwrap_or_default();

        // Process declarative watches (match prev/curr, then execute)
        let watches = self.config.watches_for_event(&event.name);
        for watch in watches {
            if watch.matches(&prev, &curr) {
                info!("Watch matched: {} -> {}", event.name, watch.exec);
                self.run_script(&watch.exec, &event).await;
            }
        }

        // Process script watches (script handles matching and action)
        let script_watches = self.config.script_watches_for_event(&event.name);
        for sw in script_watches {
            debug!("Running script watch: {} -> {}", event.name, sw.script);
            self.run_script(&sw.script, &event).await;
        }
    }

    /// Run a script with event context.
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
}

fn init_logging(level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn find_config_path() -> Option<PathBuf> {
    // Try these locations in order:
    // 1. ./middlesox.toml
    // 2. ~/.config/middlesox/config.toml
    // 3. /etc/middlesox/config.toml

    let candidates = [
        PathBuf::from("middlesox.toml"),
        dirs::config_dir()
            .map(|p| p.join("middlesox/config.toml"))
            .unwrap_or_default(),
        PathBuf::from("/etc/middlesox/config.toml"),
    ];

    candidates.into_iter().find(|p| p.exists())
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
    // Load configuration
    let config = match find_config_path() {
        Some(path) => {
            println!("Loading config from: {}", path.display());
            Config::load(&path)?
        }
        None => {
            println!("No config file found, using defaults");
            Config::default_config()
        }
    };

    // Initialize logging
    init_logging(&config.settings.log_level);

    info!("Middlesox daemon starting...");
    info!("Available built-in backends: {:?}", available_backends());
    info!("Configured adapter: {}", config.adapter.name());

    // Create the backend from adapter config
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

    // Determine scripts directory
    let scripts_dir = PathBuf::from(&config.settings.scripts_dir);
    if !scripts_dir.exists() {
        warn!("Scripts directory not found: {}", scripts_dir.display());
    }

    // Create script engine
    let engine = ScriptEngine::new(backend);

    // Create controller (note: we need a new backend for the listener)
    let listener_backend = create_backend_from_config(&config.adapter)?;

    // Extract subscribed events before moving config into controller
    let subscriptions = config.subscribed_events();
    info!("Subscribed events: {:?}", subscriptions);

    let controller = Arc::new(Controller::new(config, engine, scripts_dir));

    // Create event channel
    let (event_tx, mut event_rx) = mpsc::channel::<RawEvent>(100);

    // Spawn event listener (only listens for subscribed events)
    let listener_handle = tokio::spawn(async move {
        if let Err(e) = listener_backend.listen(event_tx, subscriptions).await {
            error!("Backend listener error: {}", e);
        }
    });

    // Spawn event processor
    let controller_handle = {
        let controller = controller.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                controller.handle_event(event).await;
            }
            info!("Event channel closed, shutting down");
        })
    };

    info!("Middlesox daemon running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");

    // Clean up
    listener_handle.abort();
    controller_handle.abort();

    info!("Middlesox daemon stopped");
    Ok(())
}
