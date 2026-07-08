//! Integration tests for the Hyprland adapter against a fake Hyprland
//! IPC server speaking the real wire protocol on temp Unix sockets.
//!
//! No live Hyprland is needed — these tests are fully hermetic.

use middlesox::ProtocolAdapter;
use middlesox_hyprland::HyprlandBackend;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, mpsc};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A fake Hyprland IPC server: `.socket.sock` answers `j/` queries with
/// canned JSON and records every request; `.socket2.sock` streams the
/// event lines pushed via [`FakeHyprland::emit`].
struct FakeHyprland {
    _dir: tempfile::TempDir,
    socket_dir: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    event_tx: mpsc::UnboundedSender<String>,
    active_window: Arc<Mutex<Value>>,
}

impl FakeHyprland {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().to_path_buf();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let active_window = Arc::new(Mutex::new(json!({
            "class": "kitty",
            "title": "Terminal",
            "floating": false,
            "fullscreen": false,
        })));

        // Command socket: one connection per request, like real Hyprland
        let cmd_listener = UnixListener::bind(socket_dir.join(".socket.sock")).unwrap();
        let requests_for_server = requests.clone();
        let window_for_server = active_window.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = cmd_listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                requests_for_server.lock().await.push(request.clone());

                let response = if request.starts_with("dispatch ") {
                    "ok".to_string()
                } else {
                    let query = request.strip_prefix("j/").unwrap_or(&request);
                    match query {
                        "version" => json!({"tag": "v0.45.0"}).to_string(),
                        "activeworkspace" => {
                            json!({"id": 3, "name": "3", "monitor": "DP-1"}).to_string()
                        }
                        "activewindow" => window_for_server.lock().await.to_string(),
                        "monitors" => json!([
                            {"name": "DP-1", "focused": true},
                            {"name": "DP-2", "focused": false},
                        ])
                        .to_string(),
                        "workspaces" => json!([
                            {"id": 1, "name": "1", "monitor": "DP-2"},
                            {"id": 3, "name": "3", "monitor": "DP-1"},
                        ])
                        .to_string(),
                        "clients" => json!([{"class": "kitty"}, {"class": "firefox"}])
                            .to_string(),
                        other => format!("unknown request: {}", other),
                    }
                };

                let _ = stream.write_all(response.as_bytes()).await;
                // Dropping the stream closes it, ending the client's read
            }
        });

        // Event socket: accept one client, stream pushed lines to it
        let event_listener = UnixListener::bind(socket_dir.join(".socket2.sock")).unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = event_listener.accept().await else {
                return;
            };
            while let Some(line) = event_rx.recv().await {
                if stream
                    .write_all(format!("{}\n", line).as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            // Channel closed: drop the stream to hang up on the client
        });

        Self {
            _dir: dir,
            socket_dir,
            requests,
            event_tx,
            active_window,
        }
    }

    fn adapter(&self) -> HyprlandBackend {
        HyprlandBackend::with_socket_dir(&self.socket_dir)
    }

    fn emit(&self, line: &str) {
        self.event_tx.send(line.to_string()).unwrap();
    }

    async fn requests(&self) -> Vec<String> {
        self.requests.lock().await.clone()
    }

    async fn set_active_window(&self, window: Value) {
        *self.active_window.lock().await = window;
    }
}

async fn next_event(
    adapter: &mut HyprlandBackend,
) -> Option<middlesox::RawEvent> {
    tokio::time::timeout(EVENT_TIMEOUT, adapter.next_event())
        .await
        .expect("timed out waiting for event")
        .expect("next_event failed")
}

#[tokio::test]
async fn init_queries_version() {
    let fake = FakeHyprland::start().await;
    let mut adapter = fake.adapter();

    adapter.init().await.unwrap();

    assert_eq!(fake.requests().await, vec!["j/version"]);
}

#[tokio::test]
async fn get_reads_hyprland_state() {
    let fake = FakeHyprland::start().await;
    let mut adapter = fake.adapter();

    assert_eq!(adapter.get("workspace").await.unwrap(), json!(3));
    assert_eq!(adapter.get("workspace_name").await.unwrap(), json!("3"));
    assert_eq!(adapter.get("title").await.unwrap(), json!("Terminal"));
    assert_eq!(adapter.get("appid").await.unwrap(), json!("kitty"));
    assert_eq!(adapter.get("fullscreen").await.unwrap(), json!(false));
    assert_eq!(adapter.get("floating").await.unwrap(), json!(false));
    assert_eq!(adapter.get("output").await.unwrap(), json!("DP-1"));
    assert_eq!(
        adapter.get("outputs").await.unwrap(),
        json!(["DP-1", "DP-2"])
    );
    assert_eq!(adapter.get("client_count").await.unwrap(), json!(2));
    assert_eq!(
        adapter.get("workspaces").await.unwrap(),
        json!([{"id": 1, "name": "1"}, {"id": 3, "name": "3"}])
    );

    assert!(adapter.get("nonsense").await.is_err());
}

#[tokio::test]
async fn get_handles_no_focused_window() {
    let fake = FakeHyprland::start().await;
    // Hyprland returns an empty object when nothing is focused
    fake.set_active_window(json!({})).await;
    let mut adapter = fake.adapter();

    assert_eq!(adapter.get("title").await.unwrap(), json!(""));
    assert_eq!(adapter.get("appid").await.unwrap(), json!(""));
    assert_eq!(adapter.get("fullscreen").await.unwrap(), json!(false));
}

#[tokio::test]
async fn set_sends_expected_dispatches() {
    let fake = FakeHyprland::start().await;
    let mut adapter = fake.adapter();

    adapter.set("workspace", json!(5)).await.unwrap();
    adapter.set("workspace", json!("web")).await.unwrap();

    // Current fullscreen is false: setting true toggles, false is a no-op
    adapter.set("fullscreen", json!(true)).await.unwrap();
    adapter.set("fullscreen", json!(false)).await.unwrap();

    // Current floating is false: setting true toggles
    adapter.set("floating", json!(true)).await.unwrap();

    let dispatches: Vec<String> = fake
        .requests()
        .await
        .into_iter()
        .filter(|r| r.starts_with("dispatch "))
        .collect();
    assert_eq!(
        dispatches,
        vec![
            "dispatch workspace 5",
            "dispatch workspace web",
            "dispatch fullscreen 0",
            "dispatch togglefloating",
        ]
    );

    // Read-only and unknown keys are rejected without touching the socket
    assert!(adapter.set("title", json!("nope")).await.is_err());
    assert!(adapter.set("nonsense", json!(1)).await.is_err());
}

#[tokio::test]
async fn events_are_translated_and_filtered_by_subscription() {
    let fake = FakeHyprland::start().await;
    let mut adapter = fake.adapter();

    let subscriptions: HashSet<String> =
        ["workspace_change", "focus_change"].map(String::from).into();
    adapter.subscribe(subscriptions).await.unwrap();

    fake.emit("workspacev2>>2,web");
    fake.emit("fullscreen>>1"); // translates, but not subscribed
    fake.emit("garbage line without separator"); // ignored
    fake.emit("activewindow>>firefox,Mozilla Firefox");
    fake.emit("workspacev2>>3,term");

    let first = next_event(&mut adapter).await.unwrap();
    assert_eq!(first.name, "workspace_change");
    assert_eq!(first.get_curr_i64("id"), Some(2));
    assert_eq!(first.get_curr_str("name"), Some("web"));

    let second = next_event(&mut adapter).await.unwrap();
    assert_eq!(second.name, "focus_change");
    assert_eq!(second.get_curr_str("appid"), Some("firefox"));
    assert_eq!(second.get_curr_str("title"), Some("Mozilla Firefox"));

    // prev state carried across events
    let third = next_event(&mut adapter).await.unwrap();
    assert_eq!(third.name, "workspace_change");
    assert_eq!(third.get_prev_i64("id"), Some(2));
    assert_eq!(third.get_prev_str("name"), Some("web"));
    assert_eq!(third.get_curr_i64("id"), Some(3));
}

#[tokio::test]
async fn event_socket_close_ends_the_stream() {
    let fake = FakeHyprland::start().await;
    let mut adapter = fake.adapter();

    adapter
        .subscribe(HashSet::from(["workspace_change".to_string()]))
        .await
        .unwrap();

    fake.emit("workspacev2>>2,web");
    assert!(next_event(&mut adapter).await.is_some());

    // Simulate Hyprland going away: the fake's event task drops the stream
    drop(fake);

    assert!(next_event(&mut adapter).await.is_none());
}

#[tokio::test]
async fn next_event_before_subscribe_errors() {
    let fake = FakeHyprland::start().await;
    let mut adapter = fake.adapter();

    assert!(adapter.next_event().await.is_err());
}

#[tokio::test]
async fn works_through_adapter_handle() {
    // Drive the adapter through the same actor loop the daemon uses:
    // init + subscribe on spawn, events via the handle's receiver,
    // get/set serialized through the command channel.
    let fake = FakeHyprland::start().await;
    let adapter: Box<dyn ProtocolAdapter> = Box::new(fake.adapter());

    let (handle, mut event_rx, task) = middlesox::AdapterHandle::spawn(
        adapter,
        HashSet::from(["workspace_change".to_string()]),
    )
    .await
    .unwrap();

    assert_eq!(handle.get("workspace").await.unwrap(), json!(3));
    handle.set("workspace", json!(7)).await.unwrap();
    assert!(fake
        .requests()
        .await
        .contains(&"dispatch workspace 7".to_string()));

    fake.emit("workspacev2>>7,code");
    let event = tokio::time::timeout(EVENT_TIMEOUT, event_rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed");
    assert_eq!(event.name, "workspace_change");
    assert_eq!(event.get_curr_i64("id"), Some(7));

    handle.shutdown().await.unwrap();
    let _ = task.await;
}
