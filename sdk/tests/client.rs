//! Mechanism-tier tests for the low-level `HarnessClient`, driving the
//! `dsh-fake-runtime` process through real pipes.

mod common;

use std::sync::Arc;
use std::time::Duration;

use deepseek_harness_sdk_rs::{HarnessClient, InitializeParams, SdkError};
use serde_json::json;

fn initialize_params() -> InitializeParams {
    InitializeParams {
        cwd: std::env::current_dir().expect("cwd"),
        provider: "test-provider".to_string(),
        model: "test-model".to_string(),
        max_tokens: Some(100),
    }
}

/// Await one notification or panic with the pending error after 5 seconds.
async fn next_notification(
    subscription: &mut deepseek_harness_sdk_rs::NotificationSubscription,
) -> deepseek_harness_sdk_rs::Notification {
    tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("notification did not arrive in time")
        .expect("subscription failed")
}

#[tokio::test]
async fn initialize_handshake_reports_server_info() {
    let client = HarnessClient::new(common::client_options("basic"));
    client.start().await.expect("start");
    let result = client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let info = result.server_info.expect("serverInfo present");
    assert_eq!(info.name.as_deref(), Some("deepseek-harness-sdk-runtime"));
    assert_eq!(info.version.as_deref(), Some("0.0.1"));
    client.close().await;
}

#[tokio::test]
async fn prompt_returns_a_message_id() {
    let client = HarnessClient::new(common::client_options("basic"));
    client.start().await.expect("start");
    let message_id = client
        .session_prompt("s1", &[json!({"type": "text", "text": "hi"})])
        .await
        .expect("session_prompt");
    assert!(!message_id.is_empty());
    client.close().await;
}

#[tokio::test]
async fn request_round_trips_through_echo() {
    let client = HarnessClient::new(common::client_options("echo"));
    client.start().await.expect("start");
    let result = client
        .request("anything", Some(json!({"key": [1, 2, 3]})))
        .await
        .expect("request");
    assert_eq!(result, json!({"key": [1, 2, 3]}));
    client.close().await;
}

#[tokio::test]
async fn request_before_start_fails_loud() {
    let client = HarnessClient::new(common::client_options("basic"));
    let error = client.request("anything", None).await.unwrap_err();
    assert!(matches!(error, SdkError::TransportClosed { .. }));
}

#[tokio::test]
async fn request_timeout_fires_after_the_deadline() {
    let mut options = common::client_options("slow");
    options.request_timeout_seconds = Some(0.2);
    options.env = Some(
        [("DSH_FAKE_DELAY_SECONDS".to_string(), "5".to_string())]
            .into_iter()
            .collect(),
    );
    let client = HarnessClient::new(options);
    client.start().await.expect("start");
    let error = client
        .session_prompt("s1", &[json!({"type": "text", "text": "hi"})])
        .await
        .unwrap_err();
    // Python reports available subprocess diagnostics with the timeout.
    match error {
        SdkError::RequestTimeout { message } => {
            assert!(message.contains("timed out waiting for DeepSeek Harness runtime"));
            assert!(
                message.contains("fake runtime slow response coming"),
                "timeout must carry the stderr tail: {message}"
            );
        }
        other => panic!("expected RequestTimeout, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test]
async fn per_call_timeout_override_replaces_the_config_for_one_request() {
    let mut options = common::client_options("slow");
    options.request_timeout_seconds = Some(0.1);
    options.env = Some(
        [("DSH_FAKE_DELAY_SECONDS".to_string(), "0.3".to_string())]
            .into_iter()
            .collect(),
    );
    let client = HarnessClient::new(options);
    client.start().await.expect("start");

    // The per-call 0.5s deadline outlives the 0.3s delayed response even
    // though the configured 0.1s deadline would not.
    let result = client
        .request_with_timeout("anything", Some(json!({"n": 1})), Some(0.5))
        .await
        .expect("per-call timeout must win");
    assert_eq!(result, json!({"n": 1}));

    // `None` keeps the configured deadline, which still fires.
    let error = client
        .request_with_timeout("anything", Some(json!({"n": 2})), None)
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::RequestTimeout { .. }));
    client.close().await;
}

#[tokio::test]
async fn negative_timeouts_clamp_to_immediate() {
    let mut options = common::client_options("echo");
    options.request_timeout_seconds = Some(-1.0);
    let client = HarnessClient::new(options);
    client
        .start()
        .await
        .expect("a negative configured timeout must not panic");
    let error = client.request("anything", None).await.unwrap_err();
    assert!(matches!(error, SdkError::RequestTimeout { .. }));
    client.close().await;
}

#[tokio::test]
async fn panicking_filters_are_contained_to_their_subscription() {
    let client = HarnessClient::new(common::client_options("ticks"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");

    let mut broken = client
        .subscribe(Some(Arc::new(|_notification| {
            panic!("bad notification filter")
        })))
        .expect("broken subscription");
    let mut healthy = client
        .subscribe(Some(Arc::new(|notification| notification.method == "tick")))
        .expect("healthy subscription");

    client.notify("emit-first", None).await.expect("notify");
    let tick = next_notification(&mut healthy).await;
    assert_eq!(
        tick.payload.get("source").and_then(|v| v.as_str()),
        Some("emit-first")
    );

    // The failure is delivered only to the broken subscription and does not
    // kill the reader or leak the notification to the unmatched queue.
    let error = tokio::time::timeout(Duration::from_secs(5), broken.next())
        .await
        .expect("broken subscription did not settle")
        .unwrap_err();
    match error {
        SdkError::Protocol { message } => {
            assert!(
                message.contains("bad notification filter"),
                "message: {message}"
            );
        }
        other => panic!("expected Protocol error, got {other:?}"),
    }
    let unmatched =
        tokio::time::timeout(Duration::from_millis(300), client.next_notification()).await;
    assert!(unmatched.is_err(), "the unmatched queue must stay empty");

    // The reader stays healthy for later traffic.
    client.notify("emit-second", None).await.expect("notify");
    let tick = next_notification(&mut healthy).await;
    assert_eq!(
        tick.payload.get("source").and_then(|v| v.as_str()),
        Some("emit-second")
    );
    client.close().await;
}

#[tokio::test]
async fn frames_with_invalid_ids_are_notifications_not_requests() {
    let client = HarnessClient::new(common::client_options("invalid-id-notification"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");

    let notification = tokio::time::timeout(Duration::from_secs(5), client.next_notification())
        .await
        .expect("notification did not arrive")
        .expect("next_notification failed");
    assert_eq!(notification.method, "tick");
    assert_eq!(notification.payload.get("n"), Some(&json!(1)));

    // The invalid id must not have queued a server-to-client request.
    let request = tokio::time::timeout(Duration::from_millis(300), client.next_request()).await;
    assert!(request.is_err(), "no request may be queued");
    client.close().await;
}

#[tokio::test]
async fn session_prompt_rejects_a_missing_message_id() {
    let client = HarnessClient::new(common::client_options("reject-prompt"));
    client.start().await.expect("start");
    let error = client
        .session_prompt("s1", &[json!({"type": "text", "text": "hi"})])
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Protocol { .. }));
    client.close().await;
}

#[tokio::test]
async fn jsonrpc_error_preserves_code_and_data() {
    let client = HarnessClient::new(common::client_options("error"));
    client.start().await.expect("start");
    let error = client
        .session_prompt("s1", &[json!({"type": "text", "text": "hi"})])
        .await
        .unwrap_err();
    match error {
        SdkError::JsonRpcResponse { code, data, .. } => {
            assert_eq!(code, Some(-32603));
            assert_eq!(data, Some(json!({"detail": "fake runtime refused"})));
        }
        other => panic!("expected JsonRpcResponse, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test]
async fn transport_closed_reports_exit_code_and_stderr_tail() {
    let client = HarnessClient::new(common::client_options("crash"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize answered before the crash");
    let error = client.request("anything", None).await.unwrap_err();
    match error {
        SdkError::TransportClosed {
            exit_code,
            stderr_tail,
            ..
        } => {
            assert_eq!(exit_code, Some(7));
            assert!(
                stderr_tail
                    .iter()
                    .any(|line| line.contains("fake runtime stderr line one"))
            );
        }
        other => panic!("expected TransportClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn close_ladder_reaps_an_uncooperative_runtime() {
    let client = HarnessClient::new(common::client_options("no-shutdown"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let started = std::time::Instant::now();
    client.close().await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "close() did not reap the process"
    );
    // A closed client rejects further requests.
    let error = client.request("anything", None).await.unwrap_err();
    assert!(matches!(error, SdkError::TransportClosed { .. }));
}

#[tokio::test]
async fn close_is_idempotent_and_start_is_memoized() {
    let client = HarnessClient::new(common::client_options("basic"));
    client.start().await.expect("start");
    client.start().await.expect("second start is a no-op");
    client.close().await;
    client.close().await;
}

#[tokio::test]
async fn subscriptions_scope_to_the_session_tree() {
    let client = HarnessClient::new(common::client_options("subagent"));
    client.start().await.expect("start");
    let root = "session-root";
    let mut tree = client
        .subscribe_session_tree(root)
        .expect("tree subscription");
    let mut unrelated = client
        .subscribe_session_tree("session-elsewhere")
        .expect("unrelated subscription");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    client
        .session_prompt(root, &[json!({"type": "text", "text": "hi"})])
        .await
        .expect("session_prompt");

    let methods: Vec<String> = {
        let mut collected = Vec::new();
        loop {
            let notification = next_notification(&mut tree).await;
            collected.push(notification.method.clone());
            let idle = notification.method == "session.status"
                && notification
                    .payload
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    == Some(root)
                && notification.payload.get("status").and_then(|v| v.as_str()) == Some("idle");
            if idle {
                break;
            }
        }
        collected
    };
    assert!(
        methods.iter().any(|m| m == "subagent.started"),
        "methods: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "subagent.finished"),
        "methods: {methods:?}"
    );
    assert!(
        methods.iter().any(|m| m == "session.event"),
        "methods: {methods:?}"
    );

    // The unrelated tree received nothing: every notification belonged to the root tree.
    assert!(
        unrelated.try_next().is_none(),
        "unrelated subscription must stay empty"
    );
    client.close().await;
}

#[tokio::test]
async fn invalid_lines_are_ignored_and_valid_frames_delivered() {
    let client = HarnessClient::new(common::client_options("invalid-lines"));
    client.start().await.expect("start");
    let mut subscription = client.subscribe(None).expect("subscribe");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize survived garbage lines");
    let notification = next_notification(&mut subscription).await;
    assert_eq!(notification.method, "session.status");
    client.close().await;
}

#[tokio::test]
async fn server_requests_queue_for_the_caller() {
    // The fake exits 0 only when the caller answers its request with the
    // expected result frame.
    let client = HarnessClient::new(common::client_options("request-to-client"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let request = client.next_request().await.expect("next_request");
    assert_eq!(request.id, json!("req-1"));
    assert_eq!(request.method, "approval/ask");
    assert_eq!(request.payload, json!({}));
    client
        .respond(&request.id, json!({"ok": true}))
        .await
        .expect("respond");
    client.close().await;
}

#[tokio::test]
async fn respond_error_round_trips_code_and_data() {
    let client = HarnessClient::new(common::client_options("request-to-client-error"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let request = client.next_request().await.expect("next_request");
    assert_eq!(
        request.id,
        json!(42),
        "the request id rides the response verbatim"
    );
    client
        .respond_error(&request.id, 99, "no", Some(json!({"detail": "test"})))
        .await
        .expect("respond_error");
    client.close().await;
}

#[tokio::test]
async fn next_request_fails_after_the_runtime_closes() {
    let client = HarnessClient::new(common::client_options("request-then-exit"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let request = client.next_request().await.expect("first request");
    assert_eq!(request.method, "approval/ask");
    let error = client.next_request().await.unwrap_err();
    match error {
        SdkError::TransportClosed { exit_code, .. } => assert_eq!(exit_code, Some(2)),
        other => panic!("expected TransportClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn next_notification_collects_unmatched_notifications() {
    let client = HarnessClient::new(common::client_options("invalid-lines"));
    client.start().await.expect("start");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let notification = tokio::time::timeout(Duration::from_secs(5), client.next_notification())
        .await
        .expect("unmatched notification did not arrive")
        .expect("next_notification failed");
    assert_eq!(notification.method, "session.status");
    client.close().await;
}

#[tokio::test]
async fn next_notification_stays_empty_when_subscribers_match() {
    let client = HarnessClient::new(common::client_options("invalid-lines"));
    client.start().await.expect("start");
    let mut subscription = client.subscribe(None).expect("subscribe");
    client
        .initialize(&initialize_params())
        .await
        .expect("initialize");
    let notification = next_notification(&mut subscription).await;
    assert_eq!(notification.method, "session.status");
    // A matched notification must not also land on the unmatched queue.
    let unmatched =
        tokio::time::timeout(Duration::from_millis(300), client.next_notification()).await;
    assert!(unmatched.is_err(), "the unmatched queue must stay empty");
    client.close().await;
}

#[tokio::test]
async fn notify_sends_a_client_notification() {
    let client = HarnessClient::new(common::client_options("basic"));
    client.start().await.expect("start");
    client
        .notify("ping", Some(json!({"n": 1})))
        .await
        .expect("notify");
    client.close().await;
}
