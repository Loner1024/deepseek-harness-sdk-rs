# DeepSeek Harness Rust SDK

[English](README.md) | 中文

[![测试](https://github.com/Loner1024/deepseek-harness-sdk-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Loner1024/deepseek-harness-sdk-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/deepseek-harness-sdk-rs.svg)](https://crates.io/crates/deepseek-harness-sdk-rs)
[![docs.rs](https://docs.rs/deepseek-harness-sdk-rs/badge.svg)](https://docs.rs/deepseek-harness-sdk-rs)

驱动 DeepSeek Harness 的 Rust SDK。SDK 把打包的 `dsh` CLI（`--profile sdk`）作为子进程启动，通过其 stdio 上的按行分帧 JSON-RPC 2.0 协议通信。CLI 及其 sidecar 随 `deepseek-harness-runtime-bin` wheel 分发，Python SDK 使用同一个 wheel；Rust 客户端是协议的 clean-room 重实现，镜像 Python SDK 的分层。

## 包

| 目录 | Crate | 职责 |
|---|---|---|
| [sdk](sdk/README.zh.md) | `deepseek-harness-sdk-rs` | 高层轮次 API、低层 JSON-RPC 客户端与运行时下载器 |

## 行为

SDK 惰性启动运行时，并在多次 `run()` 调用间持有它。托管启动要求显式 Harness home（`dsh_home` 或非空 `DSH_HOME`）；SDK 绝不隐式发现 `~/.dsh`。显式 `launch_args_override` 调用方自行负责 argv 与环境。`sdk` profile 与可选的按序 `patches` 以 `--profile`/`--patch` 参数传入。未给任何显式启动通道时，默认开启的 `runtime-download` feature 从 PyPI 风格的索引拉取平台运行时 wheel、校验摘要，并缓存 `dsh` 可执行程序、其 ripgrep sidecar 与平台资源；显式指定 `runtime_bin` 或 `command`/`args` 的调用方可以关闭该 feature。

与 Python SDK 的低层对齐包括：逐次请求超时覆盖、超时错误携带运行时 stderr 尾部诊断、`cwd`/`runtime_cwd` 的符号链接消解、通知过滤器失败隔离、带取消的原生用户提问、按需开启的审批请求、持久会话恢复、会话工作目录读写，以及 `session/wait`。初始化握手携带 `reasoningEffort`、`maxTokens` 与提问/审批的按需开关。[SDK 参考](sdk/README.zh.md)覆盖生命周期、结果、通知、运行时选择、配置与错误。

## 示例

| 示例 | 展示内容 |
|---|---|
| `simple` | 经下载器完成单任务运行；使用 `$DSH_HOME` 或临时 home。 |
| `resolve` | 通过下载器解析平台运行时并打印路径。 |
| `deep_research` | 深度研究：默认 `sdk` profile 的 `web_search`/`web_fetch` 加子代理工具，从通知流渲染子代理报告。 |

```sh
export DEEPSEEK_API_KEY=sk-your-key-here
export DSH_HOME=/absolute/path/to/isolated-dsh-home
cargo run --example deep_research -- "self-hosted vector databases"
```

## 与 DeepSeek Harness 的关系

本仓库是 Rust SDK 的独立主仓库。SDK 驱动的运行时是 [DeepSeek Harness 项目](https://github.com/deepseek-ai/deepseek-harness)以 `deepseek-harness-runtime-bin` 发布的 `dsh` CLI；SDK 遵循该运行时文档化的 stdio JSON-RPC 协议，并为测试固定自己的运行时 wheel 版本。crate 版本与其目标的 wheel 发布对齐，并采用 PyPI 的 PEP 440 拼写（`0.2.1-rc.2` → `0.2.1rc2`）。贡献以本仓库的 PR 形式进行；`deepseek-harness-sdk-rs` 的 crates.io 发布也由本仓库通过 Trusted Publishing 负责。

## 贡献者工作流

用 `cargo fmt --all`、`cargo clippy --all-targets --features test-support -- -D warnings` 与 `cargo test --features test-support` 构建和测试；`test-support` feature 构建机制层测试所驱动的 `dsh-fake-runtime` 二进制。CI 会对从 PyPI 下载的真实运行时可执行程序跑无密钥 smoke 层；本地缺少 `DSH_TEST_RUNTIME_EXE` 时该测试自行跳过。
