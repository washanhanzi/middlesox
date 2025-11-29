//! Control socket protocol for CLI ↔ daemon communication.
//!
//! The daemon listens on a Unix socket for control commands from CLI invocations.
//! Uses JSON Lines protocol for simplicity.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Get the runtime directory for sockets and lock files.
pub fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Path to the lock file.
pub fn lock_path() -> PathBuf {
    runtime_dir().join("middlesox.lock")
}

/// Path to the control socket.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("middlesox.sock")
}

/// Path to the lock file with custom runtime directory.
pub fn lock_path_in(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("middlesox.lock")
}

/// Path to the control socket with custom runtime directory.
pub fn socket_path_in(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("middlesox.sock")
}

/// Acquire an exclusive lock to prevent multiple daemon instances.
///
/// Returns the lock file handle (must be kept open to maintain lock).
pub fn acquire_lock() -> Result<File> {
    acquire_lock_in(&lock_path())
}

/// Acquire an exclusive lock at a custom path.
pub fn acquire_lock_in(path: &Path) -> Result<File> {
    let file = File::create(path)
        .with_context(|| format!("Failed to create lock file: {}", path.display()))?;

    let fd = file.as_raw_fd();
    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    if result != 0 {
        return Err(anyhow!(
            "Another instance is already running (lock file: {})",
            path.display()
        ));
    }

    // Write PID for status checks
    let mut f = &file;
    writeln!(f, "{}", std::process::id())?;

    Ok(file)
}

/// Check if a daemon is running by trying to acquire the lock.
pub fn is_daemon_running() -> bool {
    let path = lock_path();
    if !path.exists() {
        return false;
    }

    match File::open(&path) {
        Ok(file) => {
            let fd = file.as_raw_fd();
            let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                // We got the lock, so no daemon is running
                unsafe { libc::flock(fd, libc::LOCK_UN) };
                false
            } else {
                // Lock held by another process
                true
            }
        }
        Err(_) => false,
    }
}

/// Read the PID from the lock file.
pub fn read_daemon_pid() -> Option<u32> {
    let path = lock_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Control request from CLI to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Get a value from the backend.
    Get { key: String },
    /// Set a value in the backend.
    Set { key: String, value: Value },
    /// Execute a named command.
    Exec { name: String },
    /// List capabilities.
    Caps,
    /// List commands.
    Commands,
    /// Get daemon status.
    Status,
    /// Stop the daemon.
    Stop,
}

/// Control response from daemon to CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    /// True if the request succeeded.
    pub success: bool,
    /// Result value (on success).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error message (on failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    /// Create a success response with a result.
    pub fn ok(result: Value) -> Self {
        Self {
            success: true,
            result: Some(result),
            error: None,
        }
    }

    /// Create a success response without a result.
    pub fn ok_empty() -> Self {
        Self {
            success: true,
            result: None,
            error: None,
        }
    }

    /// Create an error response.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            result: None,
            error: Some(message.into()),
        }
    }
}

/// Create the control socket listener.
pub async fn create_listener() -> Result<UnixListener> {
    create_listener_at(&socket_path()).await
}

/// Create the control socket listener at a custom path.
pub async fn create_listener_at(path: &Path) -> Result<UnixListener> {
    // Remove existing socket file if present
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to remove existing socket: {}", path.display()))?;
    }

    UnixListener::bind(path)
        .with_context(|| format!("Failed to bind control socket: {}", path.display()))
}

/// Connect to the control socket.
pub async fn connect() -> Result<UnixStream> {
    connect_to(&socket_path()).await
}

/// Connect to a control socket at a custom path.
pub async fn connect_to(path: &Path) -> Result<UnixStream> {
    if !path.exists() {
        return Err(anyhow!(
            "Daemon is not running (socket not found: {})",
            path.display()
        ));
    }

    UnixStream::connect(path)
        .await
        .with_context(|| format!("Failed to connect to daemon at {}", path.display()))
}

/// Send a request to the daemon and receive a response.
pub async fn send_request(request: &ControlRequest) -> Result<ControlResponse> {
    send_request_to(&socket_path(), request).await
}

/// Send a request to a daemon at a custom socket path.
pub async fn send_request_to(
    socket_path: &Path,
    request: &ControlRequest,
) -> Result<ControlResponse> {
    let mut stream = connect_to(socket_path).await?;

    // Send request
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    // Read response
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;

    serde_json::from_str(&response_line).context("Invalid response from daemon")
}

/// Cleanup socket file on shutdown.
pub fn cleanup_socket() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
}
