//! Keyless smoke against the real runtime executable: one turn through a
//! scripted SSE model server.
//!
//! Self-skips with an explanation when `DSH_TEST_RUNTIME_EXE` or
//! `DSH_TEST_CORDIS_CONFIG` is unset; the Rust SDK CI smoke job provides both
//! from a built runtime wheel. Run locally after
//! `scripts/build-exe-for-python-sdk.ts` by exporting the built exe and the
//! checked-in default `cordis.yml`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};
use serde_json::json;

const EXPECTED: &str = "rust sdk smoke ok";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::test]
async fn real_runtime_completes_one_turn_through_a_scripted_model() {
    let (Some(exe), Some(cordis)) = (env("DSH_TEST_RUNTIME_EXE"), env("DSH_TEST_CORDIS_CONFIG"))
    else {
        eprintln!(
            "SKIP: DSH_TEST_RUNTIME_EXE and DSH_TEST_CORDIS_CONFIG are unset; the CI smoke job provides both"
        );
        return;
    };
    let session_root = tempfile::TempDir::new().expect("tempdir");
    let base = start_mock_server();

    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        runtime_bin: Some(PathBuf::from(&exe)),
        cordis: Some(PathBuf::from(&cordis)),
        base_url: Some(format!("http://{base}/v1")),
        api_key: Some("smoke-key".to_string()),
        session_root: Some(session_root.path().to_path_buf()),
        ..Default::default()
    });
    let result = tokio::time::timeout(
        Duration::from_secs(180),
        harness.run("reply exactly", &RunOptions::default()),
    )
    .await
    .expect("smoke turn timed out")
    .expect("smoke turn failed");
    assert_eq!(result.final_response, EXPECTED);
    harness.close().await;

    // The default composition persists the session log under
    // DSH_SESSION_ROOT; the file may be plain JSONL or zstd-compressed
    // depending on the persistence configuration.
    assert!(
        contains_session_log(session_root.path()),
        "session root must contain a session log"
    );
}

/// A session log exists under `dir`: any `session.jsonl` file, optionally
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
            .is_some_and(|name| name.starts_with("session.jsonl"))
        {
            return true;
        }
    }
    false
}

/// A minimal scripted model: every completion request gets the fixed text
/// completion streamed as OpenAI-style SSE, mirroring the chunk sequence of
/// the Python runtime smoke's scripted model.
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
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {"role": "assistant", "content": null, "reasoning_content": ""}}]}),
                json!({"choices": [{"delta": {"content": EXPECTED}}]}),
                json!({"choices": [{"delta": {"content": ""}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 3}}),
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
