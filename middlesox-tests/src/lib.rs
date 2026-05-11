//! Test harness for Middlesox integration tests.
//!
//! Provides utilities to run a daemon with mock backend in an isolated
//! environment for testing the full CLI/daemon flow.

use anyhow::{Context, Result, anyhow};
use middlesox::config::{AdapterConfig, Config, Settings};
use middlesox::control::{acquire_lock_in, create_listener_at, send_request_to};
use middlesox::controller::Controller;
use middlesox::AdapterHandle;
use middlesox_mock::MockBackend;
use std::fs::File;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

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
    daemon_handle: Option<JoinHandle<Result<()>>>,
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
                script_env: Default::default(),
            },
            adapter: AdapterConfig {
                name: "mock".into(),
                options: Default::default(),
            },
            watch: vec![],
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
        let backend: Box<dyn middlesox::ProtocolAdapter> = Box::new(MockBackend::new());

        let subscriptions = self.config.subscribed_events();

        // Spawn adapter actor
        let (adapter_handle, event_rx, _adapter_task) =
            AdapterHandle::spawn(backend, subscriptions).await?;

        // Create controller
        let controller = Controller::new(
            self.config.clone(),
            adapter_handle,
            self.scripts_dir.clone(),
        );

        // Create control socket
        let socket_path = self.socket_path();
        let control_listener = create_listener_at(&socket_path).await?;

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);

        // Spawn daemon task
        let handle = tokio::spawn(async move {
            controller.run(event_rx, control_listener, shutdown_rx).await
        });
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
