//! Shared controller that orchestrates the event pipeline and script execution.
//!
//! Used by both the CLI daemon and the test harness.

use crate::adapter_handle::AdapterHandle;
use crate::config::Config;
use crate::control::{ControlRequest, ControlResponse};
use crate::engine::ScriptEngine;
use crate::RawEvent;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tracing::{debug, error, info, warn};

/// Maximum number of concurrent script executions (shell and Rhai).
const MAX_CONCURRENT_SCRIPTS: usize = 8;

/// Timeout for fire-and-forget scripts triggered by events.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for synchronous script execution (user-initiated commands).
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for fire-and-forget Rhai scripts triggered by events.
const RHAI_SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum bytes of script output to log/return.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Controller orchestrates the event pipeline and script execution.
///
/// Owns the `AdapterHandle` (channel to the adapter actor task),
/// the `ScriptEngine`, config, and scripts directory.
pub struct Controller {
    config: Config,
    adapter: AdapterHandle,
    engine: Arc<ScriptEngine>,
    scripts_dir: PathBuf,
    script_semaphore: Arc<Semaphore>,
}

impl Controller {
    /// Create a new controller.
    ///
    /// The caller should have already spawned the adapter actor via
    /// `AdapterHandle::spawn()` and pass the resulting handle here.
    pub fn new(config: Config, adapter: AdapterHandle, scripts_dir: PathBuf) -> Self {
        let engine = Arc::new(ScriptEngine::new(adapter.clone()));
        Self {
            config,
            adapter,
            engine,
            scripts_dir,
            script_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SCRIPTS)),
        }
    }

    /// Get the adapter name.
    pub fn adapter_name(&self) -> &str {
        self.adapter.name()
    }

    /// Get a reference to the config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Run the main event loop.
    ///
    /// Processes events from the adapter, handles control socket connections,
    /// and waits for shutdown signals (Ctrl+C or stop command).
    pub async fn run(
        self,
        mut event_rx: mpsc::Receiver<RawEvent>,
        control_listener: UnixListener,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<()> {
        let controller = Arc::new(self);

        // Create a second shutdown channel for control-path stop requests
        let (control_shutdown_tx, mut control_shutdown_rx) = oneshot::channel::<()>();
        let shutdown_tx = Arc::new(tokio::sync::Mutex::new(Some(control_shutdown_tx)));

        info!("Daemon running. Use 'msx stop' or Ctrl+C to stop.");

        let mut run_result = Ok(());

        loop {
            tokio::select! {
                // Process events from the adapter actor
                event = event_rx.recv() => {
                    match event {
                        Some(event) => {
                            controller.handle_event(event).await;
                        }
                        None => {
                            debug!("Event channel closed");
                            run_result = Err(anyhow!("adapter event stream ended unexpectedly"));
                            break;
                        }
                    }
                }

                // Accept control socket connections
                result = control_listener.accept() => {
                    match result {
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
                            run_result = Err(e.into());
                            break;
                        }
                    }
                }

                // External shutdown (Ctrl+C or test harness)
                _ = &mut shutdown_rx => {
                    info!("External shutdown signal received");
                    break;
                }

                // Control-path stop command
                _ = &mut control_shutdown_rx => {
                    info!("Stop command received");
                    break;
                }
            }
        }

        info!("Shutting down...");
        if let Err(e) = controller.adapter.shutdown().await {
            warn!("Adapter shutdown error: {}", e);
            if run_result.is_ok() {
                run_result = Err(e);
            }
        }
        info!("Middlesox stopped");
        run_result
    }

    /// Process an incoming event.
    async fn handle_event(&self, event: RawEvent) {
        debug!(
            "Received event: {} prev={:?} curr={:?}",
            event.name, event.prev, event.curr
        );

        let watches = self.config.watches_for_event(&event.name);
        if watches.is_empty() {
            return;
        }

        let prev = event.prev.clone().unwrap_or_default();
        let curr = event.curr.clone().unwrap_or_default();

        // Process watches
        for watch in watches {
            // If no conditions specified, always run (script handles logic)
            // If conditions specified, check match first
            let should_run = (watch.prev.is_empty() && watch.curr.is_empty())
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
        if script_path.extension().is_some_and(|ext| ext == "rhai") {
            // Fire-and-forget with timeout and concurrency limit
            let engine = self.engine.clone();
            let script_name = script_name.to_string();
            let event = event.clone();
            let semaphore = self.script_semaphore.clone();
            tokio::spawn(async move {
                let _permit = match semaphore.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        error!("Script semaphore closed");
                        return;
                    }
                };
                let result = tokio::time::timeout(
                    RHAI_SCRIPT_TIMEOUT,
                    engine.execute_file_with_event(&script_path, &event),
                )
                .await;
                match result {
                    Ok(Ok(val)) => {
                        debug!("Script '{}' returned: {:?}", script_name, val);
                    }
                    Ok(Err(e)) => {
                        error!("Script '{}' failed: {}", script_name, e);
                    }
                    Err(_) => {
                        warn!(
                            "Script '{}' timed out after {}s",
                            script_name,
                            RHAI_SCRIPT_TIMEOUT.as_secs()
                        );
                    }
                }
            });
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
    ///
    /// Bounded by a concurrency semaphore and execution timeout to prevent
    /// resource exhaustion under event storms.
    async fn run_shell_script(&self, script_path: &std::path::Path, script_name: &str, event: Option<&RawEvent>) {
        debug!("Spawning command: {}", script_path.display());

        // Validate shebang for .sh files
        if script_path.extension().is_some_and(|ext| ext == "sh") {
            match has_shebang(script_path) {
                Ok(true) => {}
                Ok(false) => {
                    error!(
                        "Script '{}' is missing a shebang line (e.g., #!/bin/bash). \
                         This will cause 'Exec format error' on execution.",
                        script_path.display()
                    );
                    return;
                }
                Err(e) => {
                    error!("Failed to read script '{}': {}", script_path.display(), e);
                    return;
                }
            }
        }

        let script_name = script_name.to_string();
        let script_path = script_path.to_path_buf();
        let script_env = self.config.settings.script_env.clone();
        let semaphore = self.script_semaphore.clone();

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
            (input_json, "command".to_string(), "null".to_string(), "null".to_string())
        };

        tokio::spawn(async move {
            // Limit concurrent script executions
            let _permit = match semaphore.acquire().await {
                Ok(permit) => permit,
                Err(_) => {
                    error!("Shell semaphore closed");
                    return;
                }
            };

            let result = tokio::time::timeout(SCRIPT_TIMEOUT, async {
                let mut command = tokio::process::Command::new(&script_path);
                command
                    .arg(&input_json)
                    .envs(&script_env)
                    // Set environment variables for shell scripts.
                    // Middlesox-owned variables override configured values.
                    .env("MSX_EVENT", &env_event)
                    .env("MSX_PREV", &env_prev)
                    .env("MSX_CURR", &env_curr)
                    .kill_on_drop(true)
                    .output()
                    .await
            })
            .await;

            match result {
                Ok(Ok(output)) => {
                    if output.status.success() {
                        debug!("Command '{}' completed successfully", script_name);
                        if !output.stdout.is_empty() {
                            let stdout = &output.stdout[..output.stdout.len().min(MAX_OUTPUT_BYTES)];
                            debug!("stdout: {}", String::from_utf8_lossy(stdout));
                        }
                    } else {
                        error!(
                            "Command '{}' failed with status: {}",
                            script_name, output.status
                        );
                        if !output.stderr.is_empty() {
                            let stderr = &output.stderr[..output.stderr.len().min(MAX_OUTPUT_BYTES)];
                            error!("stderr: {}", String::from_utf8_lossy(stderr));
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("Failed to execute command '{}': {}", script_name, e);
                }
                Err(_) => {
                    warn!("Command '{}' timed out after {}s, killed", script_name, SCRIPT_TIMEOUT.as_secs());
                }
            }
        });
    }

    /// Execute a script synchronously and return the result.
    /// Used by msx exec command.
    pub async fn execute_script(&self, script_name: &str) -> Result<Option<String>, String> {
        let script_path = self.resolve_script_path(script_name);
        if !script_path.exists() {
            return Err(format!("Script not found: {}", script_path.display()));
        }

        // Check if it's a Rhai script or a shell command
        if script_path.extension().is_some_and(|ext| ext == "rhai") {
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
    ///
    /// Bounded by execution timeout to prevent runaway processes.
    async fn execute_shell_script(&self, script_path: &std::path::Path, script_name: &str) -> Result<Option<String>, String> {
        debug!("Executing command: {}", script_path.display());

        // Validate shebang for .sh files
        if script_path.extension().is_some_and(|ext| ext == "sh") {
            match has_shebang(script_path) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!(
                        "Script '{}' is missing a shebang line (e.g., #!/bin/bash)",
                        script_path.display()
                    ));
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
        let script_env = self.config.settings.script_env.clone();

        let result = tokio::time::timeout(EXEC_TIMEOUT, async {
            let mut command = tokio::process::Command::new(script_path);
            command
                .arg(&input_json)
                .envs(&script_env)
                .env("MSX_EVENT", "command")
                .env("MSX_PREV", "null")
                .env("MSX_CURR", "null")
                .kill_on_drop(true)
                .output()
                .await
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                if output.status.success() {
                    debug!("Command '{}' completed successfully", script_name);
                    let stdout = &output.stdout[..output.stdout.len().min(MAX_OUTPUT_BYTES)];
                    let stdout = String::from_utf8_lossy(stdout).to_string();
                    if stdout.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(stdout))
                    }
                } else {
                    let stderr = &output.stderr[..output.stderr.len().min(MAX_OUTPUT_BYTES)];
                    let stderr = String::from_utf8_lossy(stderr).to_string();
                    Err(format!(
                        "Command '{}' failed with status {}: {}",
                        script_name, output.status, stderr
                    ))
                }
            }
            Ok(Err(e)) => Err(format!("Failed to execute command '{}': {}", script_name, e)),
            Err(_) => Err(format!(
                "Command '{}' timed out after {}s",
                script_name,
                EXEC_TIMEOUT.as_secs()
            )),
        }
    }

    async fn validate_set_key(&self, key: &str) -> Result<(), String> {
        let manifest = self.adapter.manifest().await.map_err(|e| e.to_string())?;

        let capability = manifest
            .find(key)
            .ok_or_else(|| format!("unknown key: {}", key))?;

        if !capability.is_writable() {
            return Err(format!("read-only key: {}", key));
        }

        Ok(())
    }

    /// Handle a control request from CLI.
    pub async fn handle_control(&self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::Get { key } => match self.adapter.get(&key).await {
                Ok(value) => ControlResponse::ok(value),
                Err(e) => ControlResponse::err(e.to_string()),
            },

            ControlRequest::Set { key, value } => {
                if let Err(e) = self.validate_set_key(&key).await {
                    return ControlResponse::err(e);
                }

                match self.adapter.set(&key, value).await {
                    Ok(()) => ControlResponse::ok_empty(),
                    Err(e) => ControlResponse::err(e.to_string()),
                }
            }

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
                let manifest = match self.adapter.manifest().await {
                    Ok(m) => m,
                    Err(e) => return ControlResponse::err(e.to_string()),
                };
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
                ControlResponse::ok(serde_json::json!({
                    "running": true,
                    "pid": std::process::id(),
                    "adapter": self.adapter.name(),
                }))
            }

            ControlRequest::Stop => {
                // This is handled specially in the run() loop
                ControlResponse::ok_empty()
            }
        }
    }
}

/// Check whether a script starts with a `#!` shebang, reading only the
/// first two bytes.
fn has_shebang(path: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut buf = [0u8; 2];
    match std::fs::File::open(path)?.read_exact(&mut buf) {
        Ok(()) => Ok(&buf == b"#!"),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
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
    if is_stop
        && let Some(tx) = shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdapterConfig, Settings};
    use crate::{Capability, CapabilityManifest, ProtocolAdapter};
    use anyhow::anyhow;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PermissiveBackend {
        set_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProtocolAdapter for PermissiveBackend {
        fn name(&self) -> &str {
            "permissive"
        }

        fn manifest(&self) -> CapabilityManifest {
            CapabilityManifest::new()
                .add(Capability::read_write("workspace"))
                .add(Capability::read_only("monitor"))
        }

        async fn subscribe(&mut self, _subscriptions: HashSet<String>) -> Result<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Result<Option<RawEvent>> {
            std::future::pending::<()>().await;
            unreachable!("pending future should never resolve")
        }

        async fn get(&mut self, key: &str) -> Result<Value> {
            match key {
                "workspace" => Ok(json!(1)),
                "monitor" => Ok(json!("MON-1")),
                _ => Err(anyhow!("Unknown key: {}", key)),
            }
        }

        async fn set(&mut self, _key: &str, _value: Value) -> Result<()> {
            self.set_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn test_controller(
        set_calls: Arc<AtomicUsize>,
    ) -> Result<(Controller, AdapterHandle, tokio::task::JoinHandle<()>)> {
        let backend: Box<dyn ProtocolAdapter> = Box::new(PermissiveBackend { set_calls });
        let (adapter, _event_rx, task) = AdapterHandle::spawn(backend, HashSet::new()).await?;

        let controller = Controller::new(
            Config {
                settings: Settings::default(),
                adapter: AdapterConfig::default(),
                watch: vec![],
                command: vec![],
            },
            adapter.clone(),
            std::env::temp_dir(),
        );

        Ok((controller, adapter, task))
    }

    #[tokio::test]
    async fn cli_set_rejects_read_only_keys_before_adapter_set() {
        let set_calls = Arc::new(AtomicUsize::new(0));
        let (controller, adapter, task) = test_controller(set_calls.clone()).await.unwrap();

        let response = controller
            .handle_control(ControlRequest::Set {
                key: "monitor".into(),
                value: json!("other"),
            })
            .await;

        assert!(!response.success);
        assert_eq!(response.error.as_deref(), Some("read-only key: monitor"));
        assert_eq!(set_calls.load(Ordering::SeqCst), 0);

        adapter.shutdown().await.unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn cli_set_rejects_unknown_keys_before_adapter_set() {
        let set_calls = Arc::new(AtomicUsize::new(0));
        let (controller, adapter, task) = test_controller(set_calls.clone()).await.unwrap();

        let response = controller
            .handle_control(ControlRequest::Set {
                key: "missing".into(),
                value: json!(1),
            })
            .await;

        assert!(!response.success);
        assert_eq!(response.error.as_deref(), Some("unknown key: missing"));
        assert_eq!(set_calls.load(Ordering::SeqCst), 0);

        adapter.shutdown().await.unwrap();
        let _ = task.await;
    }

    #[tokio::test]
    async fn cli_set_allows_writable_keys() {
        let set_calls = Arc::new(AtomicUsize::new(0));
        let (controller, adapter, task) = test_controller(set_calls.clone()).await.unwrap();

        let response = controller
            .handle_control(ControlRequest::Set {
                key: "workspace".into(),
                value: json!(2),
            })
            .await;

        assert!(response.success);
        assert_eq!(set_calls.load(Ordering::SeqCst), 1);

        adapter.shutdown().await.unwrap();
        let _ = task.await;
    }
}
