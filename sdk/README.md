# deepseek-harness-sdk-rs

English | [中文](README.zh.md)

[![Tests](https://github.com/Loner1024/deepseek-harness-sdk-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Loner1024/deepseek-harness-sdk-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/deepseek-harness-sdk-rs.svg)](https://crates.io/crates/deepseek-harness-sdk-rs)
[![docs.rs](https://docs.rs/deepseek-harness-sdk-rs/badge.svg)](https://docs.rs/deepseek-harness-sdk-rs)

Rust SDK for DeepSeek Harness. The SDK spawns the packaged `dsh` CLI (`--profile sdk`) as a subprocess and drives it over the line-delimited JSON-RPC 2.0 protocol on stdio ([protocol reference](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/sdk/protocol/README.md)). The CLI and its sidecars are distributed in the `deepseek-harness-runtime-bin` wheel also used by the [Python SDK](https://github.com/deepseek-ai/deepseek-harness/blob/master/python/README.md). The crate mirrors the Python SDK's layering: [`DeepSeekHarness`](#deepseekharness) is the high-level own-runtime API, [`HarnessClient`](#harnessclient) is the low-level protocol client, and `SdkError` reproduces the Python error taxonomy.

## Platforms

macOS (arm64, x64), Linux (x64, arm64), and Windows (x64), matching the runtime wheel tags. CI runs the mechanism tier on Linux and macOS; Windows x64 is mapped but not covered by CI.

## Features

`runtime-download` (default) fetches the platform runtime wheel from a PyPI-style index, verifies its SHA-256 digest, and extracts the `dsh` executable, its ripgrep sidecar, the macOS `-spawn-helper`, and the platform resource directory into a versioned cache. Disable it when every launch is explicit.

The crate version tracks the runtime wheel release it targets, spelled as PEP 440 (`0.2.1-rc.2` → `0.2.1rc2`). `$DSH_RUNTIME_VERSION` targets a different release.

## Installation

```sh
cargo add deepseek-harness-sdk-rs@0.2.1-rc.2
```

The default `runtime-download` feature fetches the platform `dsh` runtime wheel from PyPI on first use and caches the executable and its sidecars; no system Node.js is required. The repository pins Rust 1.97.1.

## Usage

```toml
[dependencies]
deepseek-harness-sdk-rs = "0.2.1-rc.2"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Set `DEEPSEEK_API_KEY`. Pass an isolated Harness home as `dsh_home` in code, or provide a non-empty `DSH_HOME` in the environment; the example below uses the field.

```sh
export DEEPSEEK_API_KEY=sk-your-key-here
# Alternatively: export DSH_HOME=/absolute/path/to/isolated-dsh-home
```

```rust,no_run
use std::path::PathBuf;
use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};

#[tokio::main]
async fn main() -> Result<(), deepseek_harness_sdk_rs::SdkError> {
    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        dsh_home: Some(PathBuf::from("/absolute/path/to/isolated-dsh-home")),
        cwd: Some(PathBuf::from("/absolute/path/to/workspace")),
        reasoning_effort: Some("max".to_string()),
        max_tokens: Some(49_152),
        ..Default::default()
    });
    let result = harness.run("say hi", &RunOptions::default()).await?;
    println!("{}", result.final_response);
    harness.close().await;
    Ok(())
}
```

Managed launches require an explicit Harness home through `dsh_home` or a non-empty `DSH_HOME`; the SDK deliberately never discovers `~/.dsh`. The runtime subprocess starts lazily on the first `run()` and stays owned by the instance. The default configuration inherits the caller's environment, so an existing `DEEPSEEK_API_KEY` and optional `DEEPSEEK_BASE_URL` keep working; `DeepSeekHarnessConfig.env` entries overlay it for the subprocess. `base_url` and `api_key` explicitly set those two variables.

`cwd` is the agent workspace recorded on every SDK-created session; `runtime_cwd` independently selects the subprocess working directory. Both are resolved to absolute, symlink-resolved paths before launch. `provider`, `model`, optional `reasoning_effort`, and optional positive `max_tokens` are sent during initialization. `profile` defaults to `sdk`; `patches` adds ordered profile overlays.

### Sessions and resume

`harness.resume_session(saved_id)` restores persisted history from the configured home without sending input and returns a `Session` ready for `run()`. Missing, unreadable, incompatible, or occupied sessions are rejected. Save the original session id and reuse its home and workspace.

```rust,no_run
# use std::path::PathBuf;
# use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig};
# async fn example() -> Result<(), deepseek_harness_sdk_rs::SdkError> {
let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
    dsh_home: Some(PathBuf::from("/absolute/path/to/saved-home")),
    ..Default::default()
});
let session = harness.resume_session("saved-session").await?;
let result = session.run("Continue the task.", None).await?;
# harness.close().await;
# Ok(())
# }
```

`Session::get_working_directory()` reads the effective directory; `set_working_directory(path)` changes it and returns the validated absolute path. Both create an unknown Session without running a model turn. Relative paths use that Session's current directory. Origin metadata, permissions, and existing processes remain unchanged. The low-level client exposes the same methods with a leading `session_id` argument, plus `session_wait()`.

### Blocking facade

```rust,no_run
use std::path::PathBuf;
use deepseek_harness_sdk_rs::{DeepSeekHarnessConfig, DeepSeekHarnessSync, RunOptions};

let sync = DeepSeekHarnessSync::new(DeepSeekHarnessConfig {
    dsh_home: Some(PathBuf::from("/absolute/path/to/dsh-home")),
    ..Default::default()
})?;
let result = sync.run("say hi", &RunOptions::default())?;
println!("{}", result.final_response);
sync.close()?;
# Ok::<(), deepseek_harness_sdk_rs::SdkError>(())
```

`DeepSeekHarnessSync` must not be called from inside an async runtime; doing so fails with `SdkError::NestedRuntime`.

### Explicit launch and patches

`runtime_bin` selects another packaged `dsh` executable; the SDK still appends `--profile` and ordered `--patch` arguments and requires the Harness home:

```rust,no_run
use std::path::PathBuf;
use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig};

let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
    runtime_bin: Some(PathBuf::from("/path/to/deepseek-harness-sdk-runtime-linux-x64")),
    dsh_home: Some(PathBuf::from("/absolute/path/to/dsh-home")),
    profile: "sdk".to_string(),
    patches: vec![PathBuf::from("/absolute/path/to/extra.patch.yml")],
    ..Default::default()
});
```

`launch_args_override` and `command`/`args` are explicit adapters: their argv is sent verbatim, without profile or patch arguments, and they skip the `DSH_HOME` requirement. Use them for a fake runtime or a fully custom launcher.

## DeepSeekHarness

- `run(input, options)` owns one run interval: it enqueues the prompt, waits for its id in the durable `agent/inbox/spliced` receipt, and collects notifications until the agent-wide next `idle`. `RunResult.final_response` is the last committed assistant text of the interval, not a prompt-attributed answer; steering, injected context, and other queued work may contribute before idle. `finish_reason` is the `kind` of the last `turn/end`, or `None`.
- `start_session(Some(id))` opens a named session handle; `run` with no session id creates a fresh session.
- `events` carries the root-session event payloads verbatim; `notifications` adds descendant notifications discovered through `subagent.started` lineage edges. `subagent.finished` payloads carry `lastAssistantMessage` when the child produced output.
- `on_user_question` installs the native question handler; `approval_requests` opts in to server approval requests.
- `max_tokens` caps model output for SDK-created agents and their in-process descendants.

## HarnessClient

The low-level client: `start()`/`initialize()`/`session_prompt()`/`resume_session()`/`session_wait()`/`get_working_directory()`/`set_working_directory()`/`request()`/`request_with_timeout()`/`notify()`/`close()`, plus `subscribe(filter)` and `subscribe_session_tree(id)` for notifications. `request_with_timeout(method, params, timeout_seconds)` gives one call the Python SDK's per-call override: `Some(seconds)` replaces the configured `request_timeout_seconds` for that call, `None` keeps it. `next_notification()` collects notifications no subscriber matched.

`initialize()` has an independent `initialize_timeout_seconds` bound (default 30s); a timeout names the selected profile and includes retained diagnostics. A failed handshake closes the runtime.

`session_prompt()` resolves to the queued message id as soon as the runtime accepts the message, never waiting for agent activity. A panicking notification filter is contained to its own subscription and fails only that subscription's next read. `close()` runs the shutdown ladder: protocol `shutdown` → stdin EOF → SIGTERM → SIGKILL with bounded waits, then fails pending requests and subscriptions. A closed client rejects further use.

## Client approvals

Set `approval_requests: true` (or `InitializeParams::approval_requests`) only when a handler is ready to drain `next_request()` concurrently with the run. Decisions are one-shot; only `allowed-once` grants the operation.

```rust,no_run
use deepseek_harness_sdk_rs::{ApprovalOutcome, ApprovalRequestParams, ApprovalRequestResult, HarnessClient, HarnessClientOptions};

# async fn example(client: &HarnessClient) -> Result<(), deepseek_harness_sdk_rs::SdkError> {
let request = client.next_request().await?;
if request.method == "approval/request" {
    let approval: ApprovalRequestParams = serde_json::from_value(request.payload.clone())
        .map_err(|error| deepseek_harness_sdk_rs::SdkError::Protocol { message: error.to_string() })?;
    println!("{}: {:?}", approval.tool_name, approval.reason);
    client.respond(
        &request.id,
        serde_json::to_value(ApprovalRequestResult { outcome: ApprovalOutcome::Rejected }).unwrap(),
    ).await?;
}
# Ok(())
# }
```

## Native user questions

Pass `on_user_question` to `DeepSeekHarnessConfig` or `HarnessClientOptions` to answer the built-in `ask_user_question` tool through the native SDK protocol. Installing a handler advertises `capabilities.userQuestions` at initialization. The callback receives a `UserQuestionRequest` and a `UserQuestionCancellation`; return a `UserQuestionAnswer` with one item per original question id. The SDK does not choose answers.

```rust,no_run
use deepseek_harness_sdk_rs::{UserQuestionAnswer, UserQuestionAnswerItem, UserQuestionHandler};

let handler = UserQuestionHandler::new(|request, cancellation| async move {
    if cancellation.is_cancelled() {
        return Err("cancelled".to_string());
    }
    println!("{} asks {:?}", request.session_id, request.questions);
    Ok(UserQuestionAnswer {
        answers: request.questions.iter().map(|question| UserQuestionAnswerItem {
            id: question.get("id").and_then(|value| value.as_str()).unwrap_or_default().to_string(),
            selected: vec!["CSV".to_string()],
            custom: None,
        }).collect(),
    })
});
```

Callbacks run on separate tasks, so the reader keeps processing cancellation and other requests, and a callback may make further client requests. Cancellation is signalled when the runtime sends `user-question.cancelled` or the client closes; late answers are suppressed. A callback error becomes JSON-RPC `-32603`; invalid batches and duplicate request ids receive `-32602` and never reach the callback.

## Launch channels

Most explicit wins: `launch_args_override`, `command`+`args`, `runtime_bin`, `$DSH_RUNTIME_BIN`, then the downloader. Managed channels append `--profile <profile>` and `--patch <absolute path>` in order and require `dsh_home` or a non-empty `$DSH_HOME`. The downloader reads `$DSH_RUNTIME_PYPI_URL` (index override, default `https://pypi.org/pypi`), `$DSH_RUNTIME_VERSION` (wheel version, default the crate version), and `$DSH_RUNTIME_CACHE_DIR` (cache root, default the platform cache directory). There is no implicit `cordis.yml` injection or `DSH_CORDIS_CONFIG` default.

## Errors

`SdkError` reproduces the Python taxonomy: `JsonRpcResponse` (preserving `code` and `data`), `RequestTimeout` (carrying available runtime diagnostics in its message), `Protocol` (a documented-protocol violation), `TransportClosed` (exit code plus bounded stderr tail), `Io`, `RuntimeResolve` (downloader failures), `Config` (missing home or an unusable path), and `NestedRuntime` (the sync facade inside an async context).

## Development

```sh
cargo fmt --all
cargo clippy --all-targets --features test-support -- -D warnings
cargo test --features test-support
```

The `test-support` feature builds the `dsh-fake-runtime` test binary that the mechanism-tier tests drive through real pipes; run the tests with the feature enabled or the process-level tests fail with guidance. Formatting settings live in `rustfmt.toml`; Clippy enables its pedantic group, and CI denies all warnings. The keyless smoke tier (the real exe against a scripted Messages model) self-skips without `DSH_TEST_RUNTIME_EXE`; CI drives it against the runtime wheel downloaded from PyPI.

## Known limitations and deferred work

- **No protocol version negotiation** — the handshake reports `serverInfo.version` but clients do not verify it; pre-release, no compatibility promise.
- **No cancel or session-close methods** — abandoning a turn means closing the runtime process; the protocol has no prompt-cancel method.
- **No prompt-level result attribution** — `final_response` is the last assistant text of the collected interval.
- **No Office operations** — the Python SDK's `office` helpers invoke a separately downloaded Kit CLI; they are not part of the wire protocol and are not exposed by this crate.
- **No restart after close** — a closed `HarnessClient` rejects reuse, including after a failed handshake; build a new harness for a new runtime process.
- **Downloader couples to the PyPI wheel stream** — the build workflow retains platform wheels and nothing else; a wheel with a different layout breaks resolution loudly.
