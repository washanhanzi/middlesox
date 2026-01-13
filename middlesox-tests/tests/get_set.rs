//! Get/Set operation integration tests.

use middlesox_tests::{ControlRequest, TestHarness};
use serde_json::json;

#[tokio::test]
async fn test_get_workspace() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness
        .send_request(&ControlRequest::Get {
            key: "workspace".into(),
        })
        .await
        .unwrap();

    assert!(resp.success);
    // Mock backend starts at workspace 1
    assert_eq!(resp.result.unwrap(), json!(1));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_get_layout() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness
        .send_request(&ControlRequest::Get {
            key: "layout".into(),
        })
        .await
        .unwrap();

    assert!(resp.success);
    // Mock backend starts with "master" layout
    assert_eq!(resp.result.unwrap(), json!("master"));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_get_monitor() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness
        .send_request(&ControlRequest::Get {
            key: "monitor".into(),
        })
        .await
        .unwrap();

    assert!(resp.success);
    assert_eq!(resp.result.unwrap(), json!("MOCK-1"));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_set_workspace() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Set workspace to 5
    let resp = harness
        .send_request(&ControlRequest::Set {
            key: "workspace".into(),
            value: json!(5),
        })
        .await
        .unwrap();
    assert!(resp.success, "set workspace should succeed");

    // Verify the change
    let resp = harness
        .send_request(&ControlRequest::Get {
            key: "workspace".into(),
        })
        .await
        .unwrap();
    assert!(resp.success);
    assert_eq!(resp.result.unwrap(), json!(5));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_set_layout() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Set layout to "grid"
    let resp = harness
        .send_request(&ControlRequest::Set {
            key: "layout".into(),
            value: json!("grid"),
        })
        .await
        .unwrap();
    assert!(resp.success, "set layout should succeed");

    // Verify the change
    let resp = harness
        .send_request(&ControlRequest::Get {
            key: "layout".into(),
        })
        .await
        .unwrap();
    assert!(resp.success);
    assert_eq!(resp.result.unwrap(), json!("grid"));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_set_readonly_fails() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Try to set monitor (read-only)
    let resp = harness
        .send_request(&ControlRequest::Set {
            key: "monitor".into(),
            value: json!("test-monitor"),
        })
        .await
        .unwrap();

    assert!(!resp.success, "setting read-only key should fail");
    assert_eq!(resp.error.as_deref(), Some("read-only key: monitor"));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_set_unknown_key_rejected_by_manifest() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness
        .send_request(&ControlRequest::Set {
            key: "missing".into(),
            value: json!(1),
        })
        .await
        .unwrap();

    assert!(!resp.success, "setting unknown key should fail");
    assert_eq!(resp.error.as_deref(), Some("unknown key: missing"));

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_get_unknown_key() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    let resp = harness
        .send_request(&ControlRequest::Get {
            key: "nonexistent".into(),
        })
        .await
        .unwrap();

    assert!(!resp.success, "getting unknown key should fail");
    assert!(resp.error.is_some());

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_set_workspace_invalid_range() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Workspace must be 1-10 in mock backend
    let resp = harness
        .send_request(&ControlRequest::Set {
            key: "workspace".into(),
            value: json!(100),
        })
        .await
        .unwrap();

    assert!(!resp.success, "setting workspace out of range should fail");
    assert!(resp.error.is_some());

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_set_layout_invalid_value() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Layout must be one of: master, grid, float
    let resp = harness
        .send_request(&ControlRequest::Set {
            key: "layout".into(),
            value: json!("invalid_layout"),
        })
        .await
        .unwrap();

    assert!(!resp.success, "setting invalid layout should fail");
    assert!(resp.error.is_some());

    harness.stop_daemon().await.unwrap();
}

#[tokio::test]
async fn test_multiple_set_operations() {
    let mut harness = TestHarness::new().await.unwrap();
    harness.start_daemon().await.unwrap();

    // Set workspace multiple times
    for ws in [2, 3, 4, 5] {
        let resp = harness
            .send_request(&ControlRequest::Set {
                key: "workspace".into(),
                value: json!(ws),
            })
            .await
            .unwrap();
        assert!(resp.success);

        let resp = harness
            .send_request(&ControlRequest::Get {
                key: "workspace".into(),
            })
            .await
            .unwrap();
        assert_eq!(resp.result.unwrap(), json!(ws));
    }

    harness.stop_daemon().await.unwrap();
}
