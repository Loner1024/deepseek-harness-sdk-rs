//! Deep research example: one research topic, delegated subtopics, with
//! streaming rendering.
//!
//! The runtime composition ([`deep_research.cordis.yml`](deep_research.cordis.yml))
//! mounts `web_search`, `web_fetch`, and the in-process subagent tool. The
//! main agent plans the research and delegates subtopics to subagents. This
//! program renders the live notification stream: the main report streams
//! token-by-token to stdout (`assistant/chunk` text deltas), each subagent's
//! report streams on its own labeled lane; reasoning deltas are skipped and
//! lifecycle transitions are logged.
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

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

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
    let session_id =
        session_id.unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4().simple()));

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
    eprintln!("session id:    {session_id}");

    let subagent_count = std::sync::atomic::AtomicUsize::new(0);
    let stream_state = Mutex::new(StreamState::default());
    let options = RunOptions {
        session_id: Some(&session_id),
        on_notification: Some(&|notification: &Notification| {
            render_notification(notification, &stream_state, &subagent_count);
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
            // The report was already streamed; reprint it only when no chunk
            // ever reached the root lane (e.g. an immediate stop).
            let root_streamed = stream_state
                .lock()
                .unwrap()
                .lanes
                .get(&session_id)
                .is_some_and(|lane| lane.streamed);
            if !root_streamed {
                println!("\n=== final response ===\n{}\n", result.final_response);
            } else {
                println!();
            }
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

/// Per-session streaming lane: the root session renders bare on stdout;
/// subagent lanes carry a `[subagent <id>]` prefix. Block and request
/// boundaries emit line breaks so interleaved lanes stay readable.
struct Lane {
    label: Option<String>,
    step: Option<(u64, u64)>,
    block: Option<u64>,
    at_line_start: bool,
    streamed: bool,
}

impl Default for Lane {
    fn default() -> Self {
        Self {
            label: None,
            step: None,
            block: None,
            // A lane starts at the beginning of a line, so the first delta
            // prints its label once instead of before every fragment.
            at_line_start: true,
            streamed: false,
        }
    }
}

#[derive(Default)]
struct StreamState {
    lanes: HashMap<String, Lane>,
}

/// Print one research-relevant notification: token deltas stream to their
/// session lane, subagent lifecycle and status transitions log to stderr.
fn render_notification(
    notification: &Notification,
    state: &Mutex<StreamState>,
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
            let label = format!("[subagent {}] ", &child[..child.len().min(8)]);
            state.lock().unwrap().lanes.insert(
                child.to_string(),
                Lane {
                    label: Some(label),
                    ..Default::default()
                },
            );
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
            let streamed = state
                .lock()
                .unwrap()
                .lanes
                .get(child)
                .is_some_and(|lane| lane.streamed);
            if streamed {
                println!();
            } else if let Some(report) =
                payload.get("lastAssistantMessage").and_then(text_of_blocks)
            {
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
        "session.event" => {
            let event = payload.get("event");
            if event.and_then(|e| e.get("type")).and_then(Value::as_str) == Some("assistant/chunk")
            {
                render_chunk(payload, state);
            }
        }
        _ => {}
    }
}

/// Stream one `assistant/chunk` event to its session lane.
fn render_chunk(payload: &Value, state: &Mutex<StreamState>) {
    let Some(session) = payload.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let Some(event) = payload.get("event") else {
        return;
    };
    // The session-log envelope nests the chunk payload under `data`.
    let Some(data) = event.get("data") else {
        return;
    };
    let Some(chunk) = data.get("chunk") else {
        return;
    };
    let step = (
        data.get("turn").and_then(Value::as_u64).unwrap_or(0),
        data.get("step").and_then(Value::as_u64).unwrap_or(0),
    );

    let mut state = state.lock().unwrap();
    let lane = state.lanes.entry(session.to_string()).or_default();
    if lane.step != Some(step) {
        if lane.streamed && !lane.at_line_start {
            println!();
        }
        lane.step = Some(step);
        lane.block = None;
    }

    match chunk.get("type").and_then(Value::as_str) {
        Some("block-start") => {
            let index = chunk.get("index").and_then(Value::as_u64);
            if lane.streamed && !lane.at_line_start {
                println!();
            }
            lane.block = index;
        }
        Some("text-delta") => {
            let text = chunk.get("text").and_then(Value::as_str).unwrap_or("");
            emit(lane, text);
        }
        _ => {}
    }
}

/// Write one text delta to the lane: the lane label opens the line, and the
/// stream flushes per delta.
fn emit(lane: &mut Lane, text: &str) {
    if !lane.at_line_start
        && let Some(label) = &lane.label
    {
        print!("{label}");
    }
    print!("{text}");
    let _ = std::io::stdout().flush();
    lane.streamed = true;
    lane.at_line_start = text.ends_with('\n');
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
