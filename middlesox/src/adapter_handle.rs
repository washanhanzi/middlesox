//! Actor-based adapter handle for channel-mediated access to a ProtocolAdapter.
//!
//! The adapter runs in its own tokio task. All access goes through
//! `AdapterHandle` which sends commands over an mpsc channel and
//! receives replies via oneshot channels.

use crate::adapter::BoxedAdapter;
use crate::capability::CapabilityManifest;
use crate::event::RawEvent;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashSet;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

/// Commands sent from AdapterHandle to the adapter actor task.
pub enum AdapterCommand {
    Get {
        key: String,
        reply: oneshot::Sender<Result<Value>>,
    },
    Set {
        key: String,
        value: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    Manifest {
        reply: oneshot::Sender<CapabilityManifest>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

/// A cloneable handle for communicating with the adapter actor task.
#[derive(Clone)]
pub struct AdapterHandle {
    cmd_tx: mpsc::Sender<AdapterCommand>,
    name: String,
}

impl AdapterHandle {
    /// Spawn the adapter actor task.
    ///
    /// Initializes the adapter, subscribes to events, then runs a select!
    /// loop that processes both incoming commands and outgoing events.
    ///
    /// Returns the handle, an event receiver, and the task's JoinHandle.
    pub async fn spawn(
        mut adapter: BoxedAdapter,
        subscriptions: HashSet<String>,
    ) -> Result<(Self, mpsc::Receiver<RawEvent>, JoinHandle<()>)> {
        // Initialize adapter
        adapter.init().await?;

        let name = adapter.name().to_string();

        // Subscribe to events
        adapter.subscribe(subscriptions).await?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<AdapterCommand>(32);
        let (event_tx, event_rx) = mpsc::channel::<RawEvent>(100);

        let handle = Self {
            cmd_tx: cmd_tx.clone(),
            name,
        };

        let join_handle = tokio::spawn(adapter_actor_loop(adapter, cmd_rx, event_tx));

        Ok((handle, event_rx, join_handle))
    }

    /// Get the adapter name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Query a value from the adapter.
    pub async fn get(&self, key: &str) -> Result<Value> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(AdapterCommand::Get {
                key: key.to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("Adapter task closed"))?;
        reply_rx.await.map_err(|_| anyhow!("Adapter task dropped reply"))?
    }

    /// Set a value in the adapter.
    pub async fn set(&self, key: &str, value: Value) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(AdapterCommand::Set {
                key: key.to_string(),
                value,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("Adapter task closed"))?;
        reply_rx.await.map_err(|_| anyhow!("Adapter task dropped reply"))?
    }

    /// Get the capability manifest.
    pub async fn manifest(&self) -> Result<CapabilityManifest> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(AdapterCommand::Manifest { reply: reply_tx })
            .await
            .map_err(|_| anyhow!("Adapter task closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("Adapter task dropped reply"))
    }

    /// Shut down the adapter gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        // The task may already be gone; treat a closed channel as success
        if self
            .cmd_tx
            .send(AdapterCommand::Shutdown { reply: reply_tx })
            .await
            .is_err()
        {
            debug!("Adapter task already closed during shutdown");
            return Ok(());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                debug!("Adapter task dropped reply during shutdown");
                Ok(())
            }
        }
    }
}

/// The adapter actor loop.
///
/// Runs `select!` over:
/// - `adapter.next_event()` → forwards events to event_tx
/// - `cmd_rx.recv()` → processes Get/Set/Manifest/Shutdown commands
async fn adapter_actor_loop(
    mut adapter: BoxedAdapter,
    mut cmd_rx: mpsc::Receiver<AdapterCommand>,
    event_tx: mpsc::Sender<RawEvent>,
) {
    let mut shutdown_done = false;

    loop {
        tokio::select! {
            event_result = adapter.next_event() => {
                match event_result {
                    Ok(Some(event)) => {
                        if event_tx.send(event).await.is_err() {
                            debug!("Event receiver dropped, adapter actor exiting");
                            break;
                        }
                    }
                    Ok(None) => {
                        debug!("Adapter event stream ended");
                        break;
                    }
                    Err(e) => {
                        error!("Adapter next_event error: {}", e);
                        break;
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(AdapterCommand::Get { key, reply }) => {
                        let result = adapter.get(&key).await;
                        let _ = reply.send(result);
                    }
                    Some(AdapterCommand::Set { key, value, reply }) => {
                        let result = adapter.set(&key, value).await;
                        let _ = reply.send(result);
                    }
                    Some(AdapterCommand::Manifest { reply }) => {
                        let manifest = adapter.manifest();
                        let _ = reply.send(manifest);
                    }
                    Some(AdapterCommand::Shutdown { reply }) => {
                        let result = adapter.shutdown().await;
                        shutdown_done = true;
                        let _ = reply.send(result);
                        debug!("Adapter actor received shutdown, exiting");
                        break;
                    }
                    None => {
                        debug!("Command channel closed, adapter actor exiting");
                        break;
                    }
                }
            }
        }
    }

    if !shutdown_done
        && let Err(e) = adapter.shutdown().await
    {
        warn!("Adapter shutdown error: {}", e);
    }
}
