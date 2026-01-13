//! Daemon lifecycle integration tests.

use middlesox_tests::{ControlRequest, TestHarness};

#[tokio::test]
async fn test_daemon_start_and_status() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Verify daemon is running via status request
    let resp = harness
        .send_request(&ControlRequest::Status)
        .await
        .unwrap();

    assert!(resp.success, "Status request should succeed");
    let result = resp.result.unwrap();
    assert_eq!(result["running"], true);
    assert_eq!(result["adapter"], "mock");

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_daemon_stop() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Verify it's running
    assert!(harness.is_socket_available());

    // Send stop request
    let resp = harness
        .send_request(&ControlRequest::Stop)
        .await
        .unwrap();
    assert!(resp.success);

    // Wait briefly for the daemon to shut down
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify the daemon actually stopped: subsequent requests should fail
    let result = harness.send_request(&ControlRequest::Status).await;
    assert!(result.is_err(), "Daemon should have stopped after Stop request");

    // Clean up harness state
    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_daemon_caps() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness.send_request(&ControlRequest::Caps).await.unwrap();
    assert!(resp.success);

    let caps = resp.result.unwrap();
    let caps = caps.as_array().expect("caps should be an array");

    // Mock backend should have workspace, layout, monitor, window_count
    let cap_names: Vec<&str> = caps
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();

    assert!(cap_names.contains(&"workspace"), "should have workspace capability");
    assert!(cap_names.contains(&"layout"), "should have layout capability");
    assert!(cap_names.contains(&"monitor"), "should have monitor capability");
    assert!(cap_names.contains(&"window_count"), "should have window_count capability");

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_daemon_commands_empty() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness.send_request(&ControlRequest::Commands).await.unwrap();
    assert!(resp.success);

    let cmds = resp.result.unwrap();
    let cmds = cmds.as_array().expect("commands should be an array");
    assert!(cmds.is_empty(), "default config should have no commands");

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_multiple_requests() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Send multiple requests in sequence
    for _ in 0..5 {
        let resp = harness.send_request(&ControlRequest::Status).await.unwrap();
        assert!(resp.success);
    }

    harness.stop_daemon().await.unwrap();
}
