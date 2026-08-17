//! Mechanism-tier tests for the high-level `DeepSeekHarness` API, driving the
//! `dsh-fake-runtime` process through real pipes.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use deepseek_harness_sdk_rs::{
    DeepSeekHarness, DeepSeekHarnessConfig, DeepSeekHarnessSync, InitializeParams, RunOptions,
    SdkError,
};
use serde_json::json;

fn harness_config(scenario: &str) -> DeepSeekHarnessConfig {
    DeepSeekHarnessConfig {
        launch_args_override: Some(vec![common::fake_runtime(), scenario.to_string()]),
        ..Default::default()
    }
}

#[tokio::test]
async fn run_collects_one_turn() {
    let harness = DeepSeekHarness::new(harness_config("basic"));
    let result = harness
        .run("say hi", &RunOptions::default())
        .await
        .expect("run");
    assert_eq!(result.final_response, "hello from fake");
    assert_eq!(result.finish_reason.as_deref(), Some("max-tokens"));
    assert!(!result.events.is_empty());
    assert!(
        result
            .notifications
            .iter()
            .any(|n| n.method == "session.status"
                && n.payload.get("status").and_then(|v| v.as_str()) == Some("idle"))
    );
    harness.close().await;
}

#[tokio::test]
async fn run_honors_an_explicit_session_and_callback() {
    let harness = DeepSeekHarness::new(harness_config("basic"));
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let options = RunOptions {
        session_id: Some("session-fixed"),
        on_notification: Some(&move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    };
    let result = harness.run("hi", &options).await.expect("run");
    assert_eq!(result.session_id, "session-fixed");
    assert_eq!(seen.load(Ordering::SeqCst), result.notifications.len());
    harness.close().await;
}

#[tokio::test]
async fn run_collects_descendant_notifications() {
    let harness = DeepSeekHarness::new(harness_config("subagent"));
    let result = harness
        .run("hi", &RunOptions::default())
        .await
        .expect("run");
    assert_eq!(result.final_response, "hello from fake");
    assert!(
        result
            .notifications
            .iter()
            .any(|n| n.method == "subagent.started")
    );
    assert!(
        result
            .notifications
            .iter()
            .any(|n| n.method == "subagent.finished")
    );
    harness.close().await;
}

#[tokio::test]
async fn run_rejects_a_turn_end_without_reason_kind() {
    let harness = DeepSeekHarness::new(harness_config("bad-turn-end"));
    let error = harness.run("hi", &RunOptions::default()).await.unwrap_err();
    assert!(matches!(error, SdkError::Protocol { .. }));
    harness.close().await;
}

#[tokio::test]
async fn run_without_assistant_messages_is_empty() {
    let harness = DeepSeekHarness::new(harness_config("no-assistant"));
    let result = harness
        .run("hi", &RunOptions::default())
        .await
        .expect("run");
    assert_eq!(result.final_response, "");
    assert_eq!(result.finish_reason.as_deref(), Some("ok"));
    harness.close().await;
}

#[tokio::test]
async fn session_handle_runs_multiple_turns() {
    let harness = DeepSeekHarness::new(harness_config("basic"));
    let session = harness
        .start_session(Some("session-reuse"))
        .await
        .expect("start_session");
    let first = session.run("one", None).await.expect("first turn");
    let second = session.run("two", None).await.expect("second turn");
    assert_eq!(first.session_id, "session-reuse");
    assert_eq!(second.session_id, "session-reuse");
    assert_eq!(first.final_response, "hello from fake");
    assert_eq!(second.final_response, "hello from fake");
    harness.close().await;
}

#[tokio::test]
async fn explicit_content_blocks_pass_through() {
    let harness = DeepSeekHarness::new(harness_config("basic"));
    let result = harness
        .run(
            vec![json!({"type": "text", "text": "structured"})],
            &RunOptions::default(),
        )
        .await
        .expect("run");
    assert_eq!(result.final_response, "hello from fake");
    harness.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn cwd_and_runtime_cwd_resolve_before_launch_and_handshake() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::TempDir::new().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let runtime_dir = temp.path().join("runtime-dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::create_dir_all(&runtime_dir).expect("runtime dir");
    let workspace_link = temp.path().join("workspace-link");
    let runtime_link = temp.path().join("runtime-link");
    symlink(&workspace, &workspace_link).expect("workspace symlink");
    symlink(&runtime_dir, &runtime_link).expect("runtime symlink");

    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        launch_args_override: Some(vec![common::fake_runtime(), "cwd".to_string()]),
        cwd: Some(workspace_link.clone()),
        runtime_cwd: Some(runtime_link.clone()),
        ..Default::default()
    });
    harness.start().await.expect("start resolves both paths");

    // The fake `cwd` scenario reports the wire cwd, the subprocess cwd, and
    // `$DSH_CWD`; all three must be the symlink-resolved targets, matching
    // Python's `Path.resolve()` semantics.
    let report = harness
        .client()
        .initialize(&InitializeParams {
            cwd: workspace_link,
            provider: "p".to_string(),
            model: "m".to_string(),
            max_tokens: None,
        })
        .await
        .expect("second initialize");
    let version = report
        .server_info
        .expect("serverInfo")
        .version
        .expect("version");
    let report: serde_json::Value = serde_json::from_str(&version).expect("cwd report is JSON");

    let workspace = std::fs::canonicalize(&workspace).expect("canonical workspace");
    let runtime_dir = std::fs::canonicalize(&runtime_dir).expect("canonical runtime dir");
    assert_eq!(
        report.get("wire").and_then(|value| value.as_str()),
        workspace.to_str()
    );
    assert_eq!(
        report.get("process").and_then(|value| value.as_str()),
        runtime_dir.to_str()
    );
    assert_eq!(
        report.get("env").and_then(|value| value.as_str()),
        workspace.to_str()
    );
    harness.close().await;
}

#[test]
fn sync_facade_runs_a_turn() {
    let sync = DeepSeekHarnessSync::new(harness_config("basic")).expect("sync facade");
    let result = sync.run("hi", &RunOptions::default()).expect("sync run");
    assert_eq!(result.final_response, "hello from fake");
    sync.close().expect("sync close");
}

#[test]
fn sync_facade_rejects_an_async_context() {
    let sync = DeepSeekHarnessSync::new(harness_config("basic")).expect("sync facade");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let error = sync.run("hi", &RunOptions::default()).unwrap_err();
        assert!(matches!(error, SdkError::NestedRuntime));
    });
    drop(runtime);
    sync.close().expect("close outside the async context");
}
