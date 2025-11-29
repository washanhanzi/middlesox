//! Test harness for Middlesox integration tests.
//!
//! Provides utilities to run a daemon with mock backend in an isolated
//! environment for testing the full CLI/daemon flow.

use anyhow::{anyhow, Context, Result};
use middlesox::config::{AdapterConfig, Config, Settings};
use middlesox::control::{acquire_lock_in, create_listener_at, send_request_to};
use middlesox::engine::ScriptEngine;
use middlesox::{BoxedAdapter, MockBackend, RawEvent};
use std::collections::HashSet;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

// Re-export control types for test convenience
pub use middlesox::control::{ControlRequest, ControlResponse};

/// Test harness for running the daemon in isolation.
pub struct TestHarness {
    /// Temporary directory for sockets, lock files, and scripts.
    pub runtime_dir: TempDir,
    /// Scripts directory within runtime_dir.
    pub scripts_dir: PathBuf,
    /// Configuration for the test daemon.
    config: Config,
    /// Handle to the daemon task.
    daemon_handle: Option<JoinHandle<()>>,
    /// Shutdown signal sender.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Lock file (kept open to maintain lock).
    _lock_file: Option<File>,
}

impl TestHarness {
    /// Create a new test harness with default mock backend config.
    pub async fn new() -> Result<Self> {
        let runtime_dir = TempDir::new().context("Failed to create temp dir")?;
        let scripts_dir = runtime_dir.path().join("scripts");
        std::fs::create_dir_all(&scripts_dir)?;

        let config = Config {
            settings: Settings {
                scripts_dir: scripts_dir.to_string_lossy().to_string(),
                log_level: "debug".into(),
            },
            adapter: AdapterConfig::Mock,
            watch: vec![],
            watch_with_script: vec![],
            command: vec![],
        };

        Ok(Self {
            runtime_dir,
            scripts_dir,
            config,
            daemon_handle: None,
            shutdown_tx: None,
            _lock_file: None,
        })
    }

    /// Create a test harness with a custom config.
    pub async fn with_config(config: Config) -> Result<Self> {
        let runtime_dir = TempDir::new().context("Failed to create temp dir")?;
        let scripts_dir = runtime_dir.path().join("scripts");
        std::fs::create_dir_all(&scripts_dir)?;

        // Override scripts_dir to use temp directory
        let mut config = config;
        config.settings.scripts_dir = scripts_dir.to_string_lossy().to_string();

        Ok(Self {
            runtime_dir,
            scripts_dir,
            config,
            daemon_handle: None,
            shutdown_tx: None,
            _lock_file: None,
        })
    }

    /// Get the socket path for this test harness.
    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.path().join("middlesox.sock")
    }

    /// Get the lock path for this test harness.
    pub fn lock_path(&self) -> PathBuf {
        self.runtime_dir.path().join("middlesox.lock")
    }

    /// Write a script file to the scripts directory.
    pub async fn write_script(&self, name: &str, content: &str) -> Result<PathBuf> {
        let path = self.scripts_dir.join(name);
        tokio::fs::write(&path, content).await?;
        Ok(path)
    }

    /// Start the daemon.
    pub async fn start_daemon(&mut self) -> Result<()> {
        if self.daemon_handle.is_some() {
            return Err(anyhow!("Daemon already running"));
        }

        // Acquire lock
        let lock_path = self.lock_path();
        let lock_file = acquire_lock_in(&lock_path)?;
        self._lock_file = Some(lock_file);

        // Create backend
        let backend: BoxedAdapter = Box::new(MockBackend::new());
        let listener_backend: BoxedAdapter = Box::new(MockBackend::new());

        // Create engine and controller
        let engine = ScriptEngine::new(backend);
        let subscriptions = self.config.subscribed_events();
        let controller = Arc::new(Controller::new(
            self.config.clone(),
            engine,
            self.scripts_dir.clone(),
        ));

        // Create control socket
        let socket_path = self.socket_path();
        let control_listener = create_listener_at(&socket_path).await?;

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);

        // Spawn daemon task
        let handle = tokio::spawn(run_daemon_inner(
            listener_backend,
            subscriptions,
            controller,
            control_listener,
            shutdown_rx,
        ));
        self.daemon_handle = Some(handle);

        // Wait a bit for daemon to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        Ok(())
    }

    /// Stop the daemon.
    pub async fn stop_daemon(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.daemon_handle.take() {
            // Give it time to shutdown gracefully
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            handle.abort();
        }

        self._lock_file = None;
        Ok(())
    }

    /// Send a control request to the daemon.
    pub async fn send_request(&self, request: &ControlRequest) -> Result<ControlResponse> {
        let socket_path = self.socket_path();
        send_request_to(&socket_path, request).await
    }

    /// Check if the daemon socket exists.
    pub fn is_socket_available(&self) -> bool {
        self.socket_path().exists()
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Try to stop daemon if still running
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.daemon_handle.take() {
            handle.abort();
        }
    }
}

// ============================================================================
// Internal daemon implementation (mirrors middlesox-cli daemon logic)
// ============================================================================

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

    async fn handle_event(&self, event: RawEvent) {
        debug!(
            "Received event: {} prev={:?} curr={:?}",
            event.name, event.prev, event.curr
        );

        let prev = event.prev.clone().unwrap_or_default();
        let curr = event.curr.clone().unwrap_or_default();

        for watch in self.config.watches_for_event(&event.name) {
            if watch.matches(&prev, &curr) {
                info!("Watch matched: {} -> {}", event.name, watch.exec);
                self.run_script(&watch.exec, &event).await;
            }
        }

        for sw in self.config.script_watches_for_event(&event.name) {
            debug!("Running script watch: {} -> {}", event.name, sw.script);
            self.run_script(&sw.script, &event).await;
        }
    }

    async fn run_script(&self, script_name: &str, event: &RawEvent) {
        let script_path = self.scripts_dir.join(script_name);
        if !script_path.exists() {
            debug!("Script not found: {}", script_path.display());
            return;
        }

        match self.engine.execute_file_with_event(&script_path, event).await {
            Ok(result) => debug!("Script '{}' returned: {:?}", script_name, result),
            Err(e) => error!("Script '{}' failed: {}", script_name, e),
        }
    }

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

            ControlRequest::Stop => ControlResponse::ok_empty(),
        }
    }
}

async fn run_daemon_inner(
    listener_backend: BoxedAdapter,
    subscriptions: HashSet<String>,
    controller: Arc<Controller>,
    control_listener: UnixListener,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let shutdown_tx = Arc::new(tokio::sync::Mutex::new(None::<oneshot::Sender<()>>));

    // Event channel
    let (event_tx, mut event_rx) = mpsc::channel::<RawEvent>(100);

    // Spawn event listener
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
    });

    // Spawn control socket handler
    let controller_for_control = controller.clone();
    let shutdown_tx_for_control = shutdown_tx.clone();
    let control_handle = tokio::spawn(async move {
        loop {
            match control_listener.accept().await {
                Ok((stream, _)) => {
                    let controller = controller_for_control.clone();
                    let shutdown_tx = shutdown_tx_for_control.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_control_client(stream, controller, shutdown_tx).await
                        {
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
    });

    // Wait for shutdown
    let _ = shutdown_rx.await;

    // Cleanup
    listener_handle.abort();
    event_handle.abort();
    control_handle.abort();
}

async fn handle_control_client(
    stream: UnixStream,
    controller: Arc<Controller>,
    _shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    let request: ControlRequest = serde_json::from_str(line.trim())?;
    debug!("Control request: {:?}", request);

    let response = controller.handle_control(request).await;

    let mut response_line = serde_json::to_string(&response)?;
    response_line.push('\n');
    writer.write_all(response_line.as_bytes()).await?;

    Ok(())
}

