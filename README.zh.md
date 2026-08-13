# DeepSeek Harness Rust SDK

[English](README.md) | 中文

驱动 DeepSeek Harness 的 Rust SDK。SDK 把 harness 运行时作为子进程启动，通过其 stdio 上的按行分帧 JSON-RPC 2.0 协议通信。运行时是随 [Python SDK](https://github.com/deepseek-ai/deepseek-harness/blob/master/python/README.md) 分发的单文件可执行程序；Rust 客户端是协议的 clean-room 重实现，镜像 Python SDK 的分层。

## 包

| 目录 | Crate | 职责 |
|---|---|---|
| [sdk](sdk/README.md) | `deepseek-harness-sdk` | 高层轮次 API、低层 JSON-RPC 客户端与运行时下载器 |

## 行为

SDK 惰性启动运行时，并在多次 `run()` 调用间持有它。未给任何显式启动通道时，默认开启的 `runtime-download` feature 从 PyPI 风格的索引拉取平台运行时 wheel、校验摘要并缓存可执行程序；显式指定 `runtime_bin` 或 `command`/`args` 的调用方可以关闭该 feature。[SDK 参考](sdk/README.md)覆盖生命周期、结果、通知、运行时选择、配置与错误。

## 上游

本仓库是从 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`rust/` 目录）抽取的 Rust SDK。规范的贡献流程是向上游仓库提 PR；`deepseek-harness-sdk` crate 名的 crates.io 发布也归上游所有。

## 贡献者工作流

用 `cargo fmt --all`、`cargo clippy --all-targets --features test-support -- -D warnings` 与 `cargo test --features test-support` 构建和测试；`test-support` feature 构建机制层测试所驱动的 `dsh-fake-runtime` 二进制。无密钥 smoke 测试在缺少运行时环境变量时自行跳过；上游仓库的 CI 会针对真实运行时可执行程序驱动它。
