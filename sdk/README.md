# deepseek-harness-sdk

English | [中文](README.zh.md)

Rust SDK for DeepSeek Harness. The SDK spawns the harness runtime — the single-file executable also distributed with the [Python SDK](https://github.com/deepseek-ai/deepseek-harness/blob/master/python/README.md) — as a subprocess and drives it over the line-delimited JSON-RPC 2.0 protocol on its stdio ([protocol reference](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/sdk/protocol/README.md)). It mirrors the Python SDK's layering: [`DeepSeekHarness`](#deepeekharness) is the high-level own-runtime API, [`HarnessClient`](#harnessclient) is the low-level protocol client, and `SdkError` reproduces the Python error taxonomy.

## Platforms

macOS (arm64) and Linux (x64, arm64), matching the runtime wheel tags. Windows is a non-goal: no runtime wheel exists for it, and resolution fails loudly there.

## Features

`runtime-download` (default) fetches the platform runtime wheel from a PyPI-style index, verifies its SHA-256 digest, and extracts the executable (plus the macOS `-spawn-helper` and the default `cordis.yml`) into a versioned cache. Disable it when every launch is explicit.

## Usage

```rust,no_run
use deepseek_harness_sdk::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};

#[tokio::main]
async fn main() -> Result<(), deepseek_harness_sdk::SdkError> {
    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig::default());
    let result = harness.run("say hi", &RunOptions::default()).await?;
    println!("{}", result.final_response);
    harness.close().await;
    Ok(())
}
```

The runtime subprocess starts lazily on the first `run()` and stays owned by the instance. The default configuration inherits the caller's environment, so an existing `DEEPSEEK_API_KEY` and optional `DEEPSEEK_BASE_URL` keep working; `DeepSeekHarnessConfig.env` entries overlay it for the subprocess.

### Blocking facade

```rust,no_run
use deepseek_harness_sdk::{DeepSeekHarnessConfig, DeepSeekHarnessSync, RunOptions};

let sync = DeepSeekHarnessSync::new(DeepSeekHarnessConfig::default())?;
let result = sync.run("say hi", &RunOptions::default())?;
println!("{}", result.final_response);
sync.close()?;
# Ok::<(), deepseek_harness_sdk::SdkError>(())
```

`DeepSeekHarnessSync` must not be called from inside an async runtime; doing so fails with `SdkError::NestedRuntime`.

### Explicit launch

Callers who spawn their own runtime skip the downloader entirely:

```rust,no_run
use std::path::PathBuf;
use deepseek_harness_sdk::{DeepSeekHarness, DeepSeekHarnessConfig};

let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
    runtime_bin: Some(PathBuf::from("/path/to/dsh-jsonrpc-agent-pkg-macos-arm64")),
    ..Default::default()
});
```

## DeepSeekHarness

- `run(input, options)` owns one run interval: it enqueues the prompt, waits for its id in the durable `agent/inbox/spliced` receipt, and collects notifications until the agent-wide next `idle`. `RunResult.final_response` is the last committed assistant text of the interval, not a prompt-attributed answer; steering, injected context, and other queued work may contribute before idle.
- `start_session(Some(id))` opens a named session handle; `run` with no session id creates a fresh session. Reusing a harness and session id preserves the session-owned Bash state.
- `events` carries the root-session event payloads verbatim; `notifications` adds descendant notifications discovered through `subagent.started` lineage edges.
- The optional positive-integer `max_tokens` caps model output for SDK-created agents and their in-process descendants.

## HarnessClient

The low-level client: `start()`/`initialize()`/`session_prompt()`/`request()`/`notify()`/`close()`, plus `subscribe(filter)` and `subscribe_session_tree(id)` for notifications. `session_prompt()` resolves to the queued message id as soon as the runtime accepts the message, never waiting for agent activity. `close()` runs the shutdown ladder: protocol `shutdown` → stdin EOF → SIGTERM → SIGKILL with bounded waits, then fails pending requests and subscriptions.

## Launch channels

Most explicit wins: `launch_args_override`, `command`+`args`, `runtime_bin`, `$DSH_RUNTIME_BIN`, then the downloader. The downloader reads `$DSH_RUNTIME_PYPI_URL` (index override, default `https://pypi.org/pypi`), `$DSH_RUNTIME_VERSION` (wheel version, default the crate version), and `$DSH_RUNTIME_CACHE_DIR` (cache root, default the platform cache directory). The default `cordis.yml` is injected via `$DSH_CORDIS_CONFIG` only when the downloader resolved the runtime and no non-empty config exists; the runtime binary itself always requires an explicit config.

## Errors

`SdkError` reproduces the Python taxonomy: `JsonRpcResponse` (preserving `code` and `data`), `RequestTimeout`, `Protocol` (a documented-protocol violation), `TransportClosed` (exit code plus bounded stderr tail), `Io`, `RuntimeResolve` (downloader failures), and `NestedRuntime` (the sync facade inside an async context).

## Development

```sh
cargo fmt --all
cargo clippy --all-targets --features test-support -- -D warnings
cargo test --features test-support
```

The `test-support` feature builds the `dsh-fake-runtime` test binary that the mechanism-tier tests drive through real pipes; run the tests with the feature enabled or the process-level tests fail with guidance. The keyless smoke tier (the real exe against a scripted model) self-skips without `DSH_TEST_RUNTIME_EXE`; the upstream repository's CI drives it.

## Known limitations and deferred work

- **No protocol version negotiation** — the handshake reports `serverInfo.version` but clients do not verify it; pre-release, no compatibility promise.
- **No cancel or session-close methods** — abandoning a turn means closing the runtime process; the protocol has no prompt-cancel method.
- **No prompt-level result attribution** — `final_response` is the last assistant text of the collected interval.
- **server→client requests answer `-32601`** — the transport has no request handler; approval flows are future protocol work on both ends.
- **Downloader couples to the PyPI wheel stream** — the build workflow retains platform wheels and nothing else; a wheel with a different layout breaks resolution loudly.
