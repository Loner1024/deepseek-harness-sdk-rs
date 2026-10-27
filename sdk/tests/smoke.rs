//! Keyless smoke against the real runtime executable: one turn through a
//! scripted Messages model server.
//!
//! Self-skips with an explanation when `DSH_TEST_RUNTIME_EXE` is unset; the
//! Rust SDK CI smoke job resolves that executable (and its sidecars) through
//! the downloader.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};

const EXPECTED: &str = "rust sdk smoke ok";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::test]
async fn real_runtime_completes_one_turn_through_a_scripted_model() {
    let Some(exe) = env("DSH_TEST_RUNTIME_EXE") else {
        eprintln!(
            "SKIP: DSH_TEST_RUNTIME_EXE is unset; the CI smoke job resolves the runtime wheel"
        );
        return;
    };
    let home = tempfile::TempDir::new().expect("tempdir");
    let base = start_mock_server();

    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        runtime_bin: Some(PathBuf::from(&exe)),
        dsh_home: Some(home.path().to_path_buf()),
        base_url: Some(format!("http://{base}/v1")),
        api_key: Some("smoke-key".to_string()),
        max_tokens: Some(4_096),
        env: [("DSH_TELEMETRY_DISABLED".to_string(), "1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    });
    let result = tokio::time::timeout(
        Duration::from_mins(3),
        harness.run("reply exactly", &RunOptions::default()),
    )
    .await
    .expect("smoke turn timed out")
    .expect("smoke turn failed");
    assert_eq!(result.final_response, EXPECTED);
    harness.close().await;

    // The selected profile persists the session log under DSH_HOME; the file
    // may be plain JSONL or zstd-compressed depending on the release.
    assert!(
        contains_session_log(home.path()),
        "the harness home must contain a session log"
    );
}

#[tokio::test]
async fn real_runtime_resumes_a_persisted_session() {
    let Some(exe) = env("DSH_TEST_RUNTIME_EXE") else {
        eprintln!(
            "SKIP: DSH_TEST_RUNTIME_EXE is unset; the CI smoke job resolves the runtime wheel"
        );
        return;
    };
    let home = tempfile::TempDir::new().expect("tempdir");
    let base = start_mock_server();
    let config = || DeepSeekHarnessConfig {
        runtime_bin: Some(PathBuf::from(&exe)),
        dsh_home: Some(home.path().to_path_buf()),
        base_url: Some(format!("http://{base}/v1")),
        api_key: Some("smoke-key".to_string()),
        max_tokens: Some(4_096),
        env: [("DSH_TELEMETRY_DISABLED".to_string(), "1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let first = DeepSeekHarness::new(config());
    let result = tokio::time::timeout(
        Duration::from_mins(3),
        first.run("start a persisted session", &RunOptions::default()),
    )
    .await
    .expect("first turn timed out")
    .expect("first turn failed");
    first.close().await;

    let second = DeepSeekHarness::new(config());
    let session = tokio::time::timeout(
        Duration::from_mins(3),
        second.resume_session(&result.session_id),
    )
    .await
    .expect("resume timed out")
    .expect("resume failed");
    assert_eq!(session.id(), result.session_id);
    let resumed = tokio::time::timeout(
        Duration::from_mins(3),
        session.run("continue the task", None),
    )
    .await
    .expect("resumed turn timed out")
    .expect("resumed turn failed");
    assert_eq!(resumed.final_response, EXPECTED);
    second.close().await;
}

/// A session log exists under `dir`: any `session.*.jsonl` file, optionally
/// zstd-compressed, at any depth.
fn contains_session_log(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if contains_session_log(&path) {
                return true;
            }
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("session.") && name.contains(".jsonl"))
        {
            return true;
        }
    }
    false
}

/// A minimal scripted model: every completion request gets the fixed text
/// completion streamed as a `DeepSeek` Messages event stream.
fn start_mock_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            if read_request_body(&mut stream).is_err() {
                continue;
            }
            let payload = format!(
                "event: message_start\ndata: {}\n\n\
                 event: content_block_start\ndata: {}\n\n\
                 event: content_block_delta\ndata: {}\n\n\
                 event: content_block_stop\ndata: {}\n\n\
                 event: message_delta\ndata: {}\n\n\
                 event: message_stop\ndata: {}\n\n",
                serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": "smoke-response",
                        "model": "deepseek-v4-flash",
                        "usage": {"input_tokens": 3, "output_tokens": 0}
                    }
                }),
                serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": EXPECTED}
                }),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 3}
                }),
                serde_json::json!({"type": "message_stop"}),
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload,
            );
            let _ = stream.flush();
        }
    });
    format!("127.0.0.1:{port}")
}

/// Drain one HTTP request (headers plus the content-length body) so the
/// response cannot interleave with a half-read request. The body is ignored:
/// every request gets the fixed completion.
fn read_request_body(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 {
            return Err(std::io::Error::other("connection closed in headers"));
        }
        head.push(byte[0]);
        if head.len() > 65_536 {
            return Err(std::io::Error::other("request head too large"));
        }
    }
    let head = String::from_utf8_lossy(&head);
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    stream.read_exact(&mut body)
}
