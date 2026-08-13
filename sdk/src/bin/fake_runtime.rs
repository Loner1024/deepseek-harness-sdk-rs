//! Test-support fake runtime: speaks the SDK protocol on stdio.
//!
//! Built only with the `test-support` cargo feature and used by the SDK's
//! mechanism-tier tests, which locate it through `CARGO_BIN_EXE_dsh-fake-runtime`.
//!
//! Usage: `dsh-fake-runtime [scenario]` (default `basic`), or `$DSH_FAKE_SCENARIO`.
//!
//! Scenarios:
//! - `basic` — the happy path: receipt → assistant text → turn end → idle.
//! - `echo` — respond to every request with its params.
//! - `slow` — sleep `$DSH_FAKE_DELAY_SECONDS` (default 2) before responding.
//! - `error` — answer `session/prompt` with a JSON-RPC error carrying data.
//! - `crash` — write stderr lines and exit 7 right after `initialize`.
//! - `no-shutdown` — never exit on `shutdown`; waits to be killed.
//! - `request-to-client` — send a server-to-client request; expect `-32601`.
//! - `subagent` — emit subagent lifecycle around a child session, then idle.
//! - `invalid-lines` — emit garbage lines, then one valid notification.
//! - `bad-turn-end` — `turn/end` whose reason lacks a string `kind`.
//! - `no-assistant` — a settled turn with no assistant message.
//!
//! The fake is single-threaded and sequential, so it uses blocking stdio.

use std::io::{BufRead, BufReader, BufWriter, Write};

use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let scenario = std::env::args()
        .nth(1)
        .filter(|arg| !arg.starts_with("--"))
        .or_else(|| std::env::var("DSH_FAKE_SCENARIO").ok())
        .unwrap_or_else(|| "basic".to_string());

    let mut stdin = BufReader::new(std::io::stdin());
    let stdout = std::io::stdout();
    let mut stdout = BufWriter::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(object) = message.as_object() else {
            continue;
        };
        let Some(id) = object.get("id").map(|id| match id {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        }) else {
            continue;
        };
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            continue;
        };
        let params = object.get("params");

        match method {
            "initialize" => {
                let server_info = if scenario == "env" {
                    let config =
                        std::env::var("DSH_CORDIS_CONFIG").unwrap_or_else(|_| "unset".to_string());
                    json!({"serverInfo": {"name": "deepseek-harness-sdk-runtime", "version": config}})
                } else {
                    json!({"serverInfo": {"name": "deepseek-harness-sdk-runtime", "version": "0.0.1"}})
                };
                respond(&mut stdout, &id, server_info);
                match scenario.as_str() {
                    "crash" => {
                        eprintln!("fake runtime stderr line one");
                        eprintln!("fake runtime stderr line two");
                        std::process::exit(7);
                    }
                    "request-to-client" => {
                        write_line(
                            &mut stdout,
                            &json!({"jsonrpc": "2.0", "id": "req-1", "method": "approval/ask", "params": {}}),
                        );
                        // The client must answer -32601; the next line read is the response.
                        let mut response = String::new();
                        if stdin.read_line(&mut response).is_ok() {
                            let answer: Value =
                                serde_json::from_str(&response).unwrap_or(Value::Null);
                            let ok = answer
                                .get("error")
                                .and_then(|error| error.get("code"))
                                .and_then(Value::as_i64)
                                == Some(-32601);
                            std::process::exit(if ok { 0 } else { 3 });
                        }
                        std::process::exit(3);
                    }
                    "invalid-lines" => {
                        // Garbage lines must be ignored; the valid notification after them must arrive.
                        stdout.write_all(b"not json at all\n").ok();
                        stdout.write_all(b"{\n").ok();
                        stdout.write_all(b"\n").ok();
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": "s", "status": "idle"}),
                        );
                    }
                    _ => {}
                }
            }
            "session/prompt" => {
                let session_id = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or("s")
                    .to_string();
                match scenario.as_str() {
                    "echo" => respond(&mut stdout, &id, params.cloned().unwrap_or(Value::Null)),
                    "slow" => {
                        let seconds = std::env::var("DSH_FAKE_DELAY_SECONDS")
                            .ok()
                            .and_then(|v| v.parse::<f64>().ok())
                            .unwrap_or(2.0);
                        tokio::time::sleep(tokio::time::Duration::from_secs_f64(seconds)).await;
                        respond(&mut stdout, &id, json!({"messageId": "msg-slow"}));
                    }
                    "error" => respond_error(
                        &mut stdout,
                        &id,
                        json!({"code": -32603, "message": "boom", "data": {"detail": "fake runtime refused"}}),
                    ),
                    "subagent" => {
                        let message_id = uuid::Uuid::new_v4().simple().to_string();
                        let child = format!("session-child-{}", uuid::Uuid::new_v4().simple());
                        let receipt = json!({
                            "sessionId": session_id,
                            "event": {"type": "agent/inbox/spliced", "data": {"inserted": [{"id": message_id}]}}
                        });
                        notify(&mut stdout, "session.event", &receipt);
                        notify(
                            &mut stdout,
                            "subagent.started",
                            &json!({"parentSessionId": session_id, "childSessionId": child}),
                        );
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": child, "status": "running"}),
                        );
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": child, "event": {"type": "assistant/message", "data": {"message": {"content": [{"type": "text", "text": "child work"}]}}}}),
                        );
                        notify(
                            &mut stdout,
                            "subagent.finished",
                            &json!({
                                "provider": "fake",
                                "agentId": child,
                                "parentSessionId": session_id,
                                "childSessionId": child,
                                "status": "ok",
                                "stopReason": "end-turn",
                                "lastAssistantMessage": [{"type": "text", "text": "child result"}]
                            }),
                        );
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": child, "status": "idle"}),
                        );
                        let assistant = json!({
                            "sessionId": session_id,
                            "event": {"type": "assistant/message", "data": {"message": {"content": [{"type": "text", "text": "hello from fake"}]}}}
                        });
                        notify(&mut stdout, "session.event", &assistant);
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "turn/end", "data": {"reason": {"kind": "max-tokens"}}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": session_id, "status": "idle"}),
                        );
                        respond(&mut stdout, &id, json!({"messageId": message_id}));
                    }
                    "bad-turn-end" => {
                        let message_id = uuid::Uuid::new_v4().simple().to_string();
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "agent/inbox/spliced", "data": {"inserted": [{"id": message_id}]}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "turn/end", "data": {"reason": null}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": session_id, "status": "idle"}),
                        );
                        respond(&mut stdout, &id, json!({"messageId": message_id}));
                    }
                    "no-assistant" => {
                        let message_id = uuid::Uuid::new_v4().simple().to_string();
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "agent/inbox/spliced", "data": {"inserted": [{"id": message_id}]}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "turn/end", "data": {"reason": {"kind": "ok"}}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": session_id, "status": "idle"}),
                        );
                        respond(&mut stdout, &id, json!({"messageId": message_id}));
                    }
                    _ => {
                        let message_id = uuid::Uuid::new_v4().simple().to_string();
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "agent/inbox/spliced", "data": {"inserted": [{"id": message_id}]}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "assistant/message", "data": {"message": {"content": [{"type": "text", "text": "hello from fake"}]}}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.event",
                            &json!({"sessionId": session_id, "event": {"type": "turn/end", "data": {"reason": {"kind": "max-tokens"}}}}),
                        );
                        notify(
                            &mut stdout,
                            "session.status",
                            &json!({"sessionId": session_id, "status": "idle"}),
                        );
                        respond(&mut stdout, &id, json!({"messageId": message_id}));
                    }
                }
            }
            "shutdown" => {
                if scenario == "no-shutdown" {
                    eprintln!("fake runtime got shutdown but refuses to exit");
                    respond(&mut stdout, &id, json!({}));
                    let pending = std::sync::Arc::new(tokio::sync::Notify::new());
                    pending.notified().await;
                } else {
                    respond(&mut stdout, &id, json!({}));
                    let _ = stdout.flush();
                    return;
                }
            }
            _ => {
                if scenario == "echo" {
                    respond(&mut stdout, &id, params.cloned().unwrap_or(Value::Null));
                } else {
                    respond_error(
                        &mut stdout,
                        &id,
                        json!({"code": -32601, "message": "unknown method"}),
                    );
                }
            }
        }
    }
}

fn write_line(stdout: &mut BufWriter<std::io::Stdout>, frame: &Value) {
    let mut line = serde_json::to_string(frame).expect("json serialization cannot fail");
    line.push('\n');
    stdout.write_all(line.as_bytes()).ok();
    stdout.flush().ok();
}

fn respond(stdout: &mut BufWriter<std::io::Stdout>, id: &str, result: Value) {
    write_line(
        stdout,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn respond_error(stdout: &mut BufWriter<std::io::Stdout>, id: &str, error: Value) {
    write_line(stdout, &json!({"jsonrpc": "2.0", "id": id, "error": error}));
}

fn notify(stdout: &mut BufWriter<std::io::Stdout>, method: &str, payload: &Value) {
    write_line(
        stdout,
        &json!({"jsonrpc": "2.0", "method": method, "params": payload}),
    );
}
