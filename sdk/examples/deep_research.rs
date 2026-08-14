//! Deep research example: one research topic, delegated subtopics.
//!
//! The runtime composition ([`deep_research.cordis.yml`](deep_research.cordis.yml))
//! mounts `web_search`, `web_fetch`, and the in-process subagent tool. The
//! main agent plans the research and delegates subtopics to subagents; this
//! program renders the live notification stream (subagent lifecycle, session
//! status, subagent reports) and prints the synthesized final report.
//!
//! ```sh
//! export DEEPSEEK_API_KEY=sk-your-key-here
//! cargo run --package deepseek-harness-sdk-rs --example deep_research -- \
//!   "self-hosted vector databases"
//! ```
//!
//! Optional flags: `--session <id>` (reuse a session), `--runtime-bin <path>`
//! (explicit runtime executable; the downloader resolves it otherwise).
//! `DSH_SYSTEM_PROMPT` overrides the research persona.

use std::path::PathBuf;

use deepseek_harness_sdk_rs::{
    DeepSeekHarness, DeepSeekHarnessConfig, Notification, RunOptions, SdkError,
};
use serde_json::Value;

/// The research persona injected through `$DSH_SYSTEM_PROMPT` unless the
/// caller already set one.
const DEFAULT_PERSONA: &str = "You are a thorough research agent. Break every topic into focused \
     subtopics, delegate each to a subagent, and synthesize a final report with sources. Prefer \
     web_search and web_fetch for facts and cite URLs.";

#[tokio::main]
async fn main() {
    let (topic, session_id, runtime_bin) = parse_args();

    let cordis = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/deep_research.cordis.yml"
    ));
    let session_root =
        std::env::temp_dir().join(format!("deep-research-{}", uuid::Uuid::new_v4().simple()));
    if let Err(error) = std::fs::create_dir_all(&session_root) {
        eprintln!(
            "error: cannot create session root {}: {error}",
            session_root.display()
        );
        std::process::exit(1);
    }

    let mut env = std::collections::HashMap::new();
    if std::env::var("DSH_SYSTEM_PROMPT").is_err() {
        env.insert("DSH_SYSTEM_PROMPT".to_string(), DEFAULT_PERSONA.to_string());
    }

    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        model: "deepseek-v4-flash".to_string(),
        max_tokens: Some(16_384),
        cordis: Some(cordis),
        session_root: Some(session_root.clone()),
        runtime_bin,
        env,
        ..Default::default()
    });

    eprintln!("research topic: {topic}");
    eprintln!("session root:  {}", session_root.display());
    eprintln!(
        "session id:    {}",
        session_id.as_deref().unwrap_or("<fresh>")
    );

    let subagent_count = std::sync::atomic::AtomicUsize::new(0);
    let options = RunOptions {
        session_id: session_id.as_deref(),
        on_notification: Some(&|notification: &Notification| {
            render_notification(notification, &subagent_count);
        }),
    };
    let task = format!(
        "Research this topic in depth: {topic}\n\n\
         Plan: break the topic into 3-6 focused subtopics. Delegate each subtopic to a \
         subagent with the subagent tool, instructing it to use web_search and web_fetch \
         and to return a detailed summary with sources. Then synthesize a final report \
         with a clear structure and source citations."
    );

    match harness.run(task, &options).await {
        Ok(result) => {
            println!("\n=== final response ===\n{}\n", result.final_response);
            eprintln!(
                "finish_reason: {:?} | events: {} | notifications: {} | subagents: {}",
                result.finish_reason,
                result.events.len(),
                result.notifications.len(),
                subagent_count.load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        Err(SdkError::TransportClosed { message, .. }) => {
            eprintln!("the runtime closed before the turn settled:\n{message}");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("run failed: {error}");
            std::process::exit(1);
        }
    }
    harness.close().await;
}

fn parse_args() -> (String, Option<String>, Option<PathBuf>) {
    let mut args = std::env::args().skip(1);
    let mut topic = None;
    let mut session_id = None;
    let mut runtime_bin = None;
    let fail = |message: String| -> ! {
        eprintln!("error: {message}");
        eprintln!(
            "usage: cargo run --example deep_research -- [--session <id>] [--runtime-bin <path>] <topic>"
        );
        std::process::exit(2);
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session" => {
                session_id = args
                    .next()
                    .or_else(|| fail("--session needs a value".into()))
            }
            "--runtime-bin" => {
                runtime_bin = args
                    .next()
                    .map(PathBuf::from)
                    .or_else(|| fail("--runtime-bin needs a value".into()))
            }
            flag if flag.starts_with("--") => fail(format!("unknown flag {flag}")),
            other if topic.is_none() => topic = Some(other.to_string()),
            _ => fail("only one topic argument is accepted".into()),
        }
    }
    let topic = topic.unwrap_or_else(|| fail("no topic given".into()));
    (topic, session_id, runtime_bin)
}

/// Print one research-relevant notification: subagent lifecycle plus the
/// child's report, and whole-agent status transitions.
fn render_notification(
    notification: &Notification,
    subagent_count: &std::sync::atomic::AtomicUsize,
) {
    let payload = &notification.payload;
    match notification.method.as_str() {
        "subagent.started" => {
            let child = payload
                .get("childSessionId")
                .and_then(Value::as_str)
                .unwrap_or("?");
            eprintln!("⤷ subagent started: {child}");
        }
        "subagent.finished" => {
            subagent_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let child = payload
                .get("childSessionId")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let status = payload.get("status").and_then(Value::as_str).unwrap_or("?");
            let reason = payload
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("?");
            eprintln!("⤷ subagent finished: {child} ({status}, {reason})");
            if let Some(report) = payload.get("lastAssistantMessage").and_then(text_of_blocks) {
                eprintln!("  ── subagent report ──");
                for line in report.lines() {
                    eprintln!("  {line}");
                }
                eprintln!("  ──────────────────────");
            }
        }
        "session.status" => {
            let session = payload
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let status = payload.get("status").and_then(Value::as_str).unwrap_or("?");
            eprintln!("· session {session}: {status}");
        }
        _ => {}
    }
}

/// Concatenate the text blocks of an assistant content array.
fn text_of_blocks(blocks: &Value) -> Option<String> {
    let mut parts = String::new();
    for block in blocks.as_array()? {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            parts.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""));
        }
    }
    Some(parts)
}
