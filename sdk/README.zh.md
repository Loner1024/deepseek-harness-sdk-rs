# deepseek-harness-sdk-rs

[English](README.md) | 中文

DeepSeek Harness 的 Rust SDK。SDK 把 harness 运行时——与 [Python SDK](https://github.com/deepseek-ai/deepseek-harness/blob/master/python/README.md) 同源分发的单文件可执行程序——作为子进程启动，并在其 stdio 上以按行分帧 JSON-RPC 2.0 协议驱动它（[协议参考](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/sdk/protocol/README.md)）。分层镜像 Python SDK：[`DeepSeekHarness`](#deepeekharness) 是高层自有运行 API，[`HarnessClient`](#harnessclient) 是低层协议客户端，`SdkError` 复现 Python 错误分类。

## 平台

macOS（arm64）与 Linux（x64、arm64），与运行时 wheel 标签一致。Windows 是非目标：没有对应的运行时 wheel，解析会响亮失败。

## Features

`runtime-download`（默认开启）从 PyPI 风格的索引拉取平台运行时 wheel，校验其 SHA-256 摘要，并把可执行程序（以及 macOS 的 `-spawn-helper` 与默认 `cordis.yml`）解包进按版本命名的缓存。所有启动都显式指定时可关闭。

## 用法

```rust,no_run
use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};

#[tokio::main]
async fn main() -> Result<(), deepseek_harness_sdk_rs::SdkError> {
    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig::default());
    let result = harness.run("say hi", &RunOptions::default()).await?;
    println!("{}", result.final_response);
    harness.close().await;
    Ok(())
}
```

运行时子进程在首次 `run()` 时惰性启动，并归该实例所有。默认配置继承调用方环境，因此已有的 `DEEPSEEK_API_KEY` 与可选的 `DEEPSEEK_BASE_URL` 继续生效；`DeepSeekHarnessConfig.env` 条目为子进程覆盖它们。

### 阻塞门面

```rust,no_run
use deepseek_harness_sdk_rs::{DeepSeekHarnessConfig, DeepSeekHarnessSync, RunOptions};

let sync = DeepSeekHarnessSync::new(DeepSeekHarnessConfig::default())?;
let result = sync.run("say hi", &RunOptions::default())?;
println!("{}", result.final_response);
sync.close()?;
# Ok::<(), deepseek_harness_sdk_rs::SdkError>(())
```

`DeepSeekHarnessSync` 不得在异步运行时上下文内调用；否则以 `SdkError::NestedRuntime` 失败。

### 显式启动

自行启动运行时的调用方完全跳过下载器：

```rust,no_run
use std::path::PathBuf;
use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig};

let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
    runtime_bin: Some(PathBuf::from("/path/to/dsh-jsonrpc-agent-pkg-macos-arm64")),
    ..Default::default()
});
```

## DeepSeekHarness

- `run(input, options)` 拥有一个活动区间：把提示词入队，等它的 id 出现在持久 `agent/inbox/spliced` 回执中，收集通知直到整 agent 下一次 `idle`。`RunResult.final_response` 是该区间最后提交的助手文本，并非归因于该提示词的答案；steering、注入的上下文与其他排队工作都可能在 idle 前参与其中。
- `start_session(Some(id))` 打开具名会话句柄；`run` 未给会话 id 时创建全新会话。复用同一 harness 与会话 id 保留会话所属的 Bash 状态。
- `events` 原样携带根会话事件载荷；`notifications` 还包括经 `subagent.started` 血缘边发现的后代通知。
- 可选正整数 `max_tokens` 限制 SDK 创建的 agent 及其进程内后代的模型输出。

## HarnessClient

低层客户端：`start()`/`initialize()`/`session_prompt()`/`request()`/`notify()`/`close()`，加上 `subscribe(filter)` 与 `subscribe_session_tree(id)` 通知订阅。`next_notification()` 收集没有订阅者匹配的通知。server→client 请求排队等待 `next_request()`，并以 `respond(id, result)` 或 `respond_error(id, code, message, data)` 应答——为审批流预留的接口，镜像 Python SDK。`session_prompt()` 在运行时接受消息后立即解析为入队消息 id，绝不等待 agent 活动。`close()` 执行关闭阶梯：协议 `shutdown` → stdin EOF → SIGTERM → SIGKILL，各带有限等待，然后让挂起请求与订阅失败。

## 启动通道

最显式者优先：`launch_args_override`、`command`+`args`、`runtime_bin`、`$DSH_RUNTIME_BIN`，最后是下载器。crate 版本与其目标的运行时 wheel 发布对齐，并以 PEP 440 拼写（`0.1.0-rc.6` → `0.1.0rc6`）；`$DSH_RUNTIME_VERSION` 指向其他发布。下载器读取 `$DSH_RUNTIME_PYPI_URL`（索引覆盖，默认 `https://pypi.org/pypi`）、`$DSH_RUNTIME_VERSION`（wheel 版本，默认 crate 版本）与 `$DSH_RUNTIME_CACHE_DIR`（缓存根目录，默认平台缓存目录）。只有当下载器解析出运行时且不存在非空配置时，默认 `cordis.yml` 才经 `$DSH_CORDIS_CONFIG` 注入；运行时二进制本身始终要求显式配置。

## 错误

`SdkError` 复现 Python 分类：`JsonRpcResponse`（保留 `code` 与 `data`）、`RequestTimeout`、`Protocol`（文档化协议违例）、`TransportClosed`（退出码加有界 stderr 尾部）、`Io`、`RuntimeResolve`（下载器失败）与 `NestedRuntime`（异步上下文内调用同步门面）。

## 开发

```sh
cargo fmt --all
cargo clippy --all-targets --features test-support -- -D warnings
cargo test --features test-support
```

`test-support` feature 构建 `dsh-fake-runtime` 测试二进制，机制层测试经真实管道驱动它；必须带该 feature 运行测试，否则进程级测试会以指引信息失败。无密钥 smoke 层（真实 exe 对脚本化模型）在缺少 `DSH_TEST_RUNTIME_EXE` 时自行跳过；CI 会对从 PyPI 下载的运行时 wheel 驱动它。

## 已知限制与暂缓事项

- **无协议版本协商**——握手报告 `serverInfo.version` 但客户端不校验；处于预发布阶段，无兼容承诺。
- **无取消与会话关闭方法**——放弃轮次意味着关闭运行时进程；协议没有提示词取消方法。
- **无提示词级结果归属**——`final_response` 是所收集区间的最后一条助手文本。
- **今天没有 server→client 请求**——运行时从不发送；客户端把任何到达的请求排队等待 `respond`/`respond_error`，即预留的审批流接口。
- **下载器耦合 PyPI wheel 流**——构建工作流只保留平台 wheel；布局不同的 wheel 会让解析响亮失败。
