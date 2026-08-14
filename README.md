# DeepSeek Harness Rust SDK

English | [中文](README.zh.md)

Rust SDK for driving DeepSeek Harness. The SDK spawns the harness runtime as a subprocess and speaks the line-delimited JSON-RPC 2.0 protocol over its stdio. The runtime is the single-file executable distributed with the [Python SDK](https://github.com/deepseek-ai/deepseek-harness/blob/master/python/README.md); the Rust client is a clean-room reimplementation of the protocol, mirroring the Python SDK's layering.

## Packages

| Directory | Crate | Responsibility |
|---|---|---|
| [sdk](sdk/README.md) | `deepseek-harness-sdk-rs` | High-level turn API, low-level JSON-RPC client, and the runtime downloader |

## Behavior

The SDK launches the runtime lazily and owns it across `run()` calls. With no explicit launch channel, the `runtime-download` feature (default) fetches the platform runtime wheel from a PyPI-style index, verifies its digest, and caches the executable; explicit `runtime_bin` or `command`/`args` callers can disable the feature. [SDK reference](sdk/README.md) covers lifecycle, results, notifications, runtime selection, configuration, and errors.

## Examples

| Example | What it shows |
|---|---|
| `simple` | Zero-config one-task run. |
| `resolve` | Resolve the platform runtime through the downloader and print its paths. |
| `deep_research` | Deep research: `web_search`/`web_fetch` plus the subagent tool, with the report streamed token-by-token from `assistant/chunk` events. |

```sh
export DEEPSEEK_API_KEY=sk-your-key-here
cargo run --example deep_research -- "self-hosted vector databases"
```

## Relationship to DeepSeek Harness

This repository is the standalone home of the Rust SDK. The runtime it drives is the single-file executable published as `deepseek-harness-runtime-bin` by the [DeepSeek Harness project](https://github.com/deepseek-ai/deepseek-harness); the SDK speaks that runtime's documented stdio JSON-RPC protocol and pins its own version of the runtime wheel for testing. Contributions land as pull requests to this repository, which also owns crates.io publication of `deepseek-harness-sdk-rs` through Trusted Publishing.

## Contributor workflow

Build and test with `cargo fmt --all`, `cargo clippy --all-targets --features test-support -- -D warnings`, and `cargo test --features test-support`; the `test-support` feature builds the `dsh-fake-runtime` binary the mechanism-tier tests drive. CI runs the keyless smoke tier against the real runtime executable downloaded from PyPI; locally it self-skips without the runtime environment variables.
