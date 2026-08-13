# DeepSeek Harness Rust SDK

English | [中文](README.zh.md)

Rust SDK for driving DeepSeek Harness. The SDK spawns the harness runtime as a subprocess and speaks the line-delimited JSON-RPC 2.0 protocol over its stdio. The runtime is the single-file executable distributed with the [Python SDK](https://github.com/deepseek-ai/deepseek-harness/blob/master/python/README.md); the Rust client is a clean-room reimplementation of the protocol, mirroring the Python SDK's layering.

## Packages

| Directory | Crate | Responsibility |
|---|---|---|
| [sdk](sdk/README.md) | `deepseek-harness-sdk` | High-level turn API, low-level JSON-RPC client, and the runtime downloader |

## Behavior

The SDK launches the runtime lazily and owns it across `run()` calls. With no explicit launch channel, the `runtime-download` feature (default) fetches the platform runtime wheel from a PyPI-style index, verifies its digest, and caches the executable; explicit `runtime_bin` or `command`/`args` callers can disable the feature. [SDK reference](sdk/README.md) covers lifecycle, results, notifications, runtime selection, configuration, and errors.

## Upstream

This repository is the Rust SDK extracted from [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) (`rust/`). The canonical contribution flow is a pull request to the upstream repository, which also owns crates.io publication of the `deepseek-harness-sdk` crate name.

## Contributor workflow

Build and test with `cargo fmt --all`, `cargo clippy --all-targets --features test-support -- -D warnings`, and `cargo test --features test-support`; the `test-support` feature builds the `dsh-fake-runtime` binary the mechanism-tier tests drive. The keyless smoke test self-skips without the runtime environment variables; the upstream repository's CI drives it against the real runtime executable.
