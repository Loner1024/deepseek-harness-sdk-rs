# deepseek-harness-sdk-rs

[English](README.md) | 中文

DeepSeek Harness 的 Rust SDK。SDK 把打包的 `dsh` CLI（`--profile sdk`）作为子进程启动，并在其 stdio 上以按行分帧 JSON-RPC 2.0 协议驱动它（[协议参考](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/sdk/protocol/README.md)）。CLI 及其 sidecar 随 `deepseek-harness-runtime-bin` wheel 分发，Python SDK 使用同一个 wheel。分层镜像 Python SDK：[`DeepSeekHarness`](#deepseekharness) 是高层自有运行 API，[`HarnessClient`](#harnessclient) 是低层协议客户端，`SdkError` 复现 Python 错误分类。

## 平台

macOS（arm64、x64）、Linux（x64、arm64）与 Windows（x64），与运行时 wheel 标签一致。CI 在 Linux 与 macOS 跑机制层；Windows x64 已做映射，但未被 CI 覆盖。

## Features

`runtime-download`（默认开启）从 PyPI 风格的索引拉取平台运行时 wheel，校验其 SHA-256 摘要，并把 `dsh` 可执行程序、ripgrep sidecar、macOS `-spawn-helper` 与平台资源目录解包进按版本命名的缓存。所有启动都显式指定时可关闭。

crate 版本与其目标的运行时 wheel 发布对齐，并以 PEP 440 拼写（`0.2.1-rc.2` → `0.2.1rc2`）；`$DSH_RUNTIME_VERSION` 指向其他发布。

## 用法

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

托管启动要求显式 Harness home（`dsh_home` 或非空 `DSH_HOME`）；SDK 故意不发现 `~/.dsh`。运行时子进程在首次 `run()` 时惰性启动，并归该实例所有。默认配置继承调用方环境，因此已有的 `DEEPSEEK_API_KEY` 与可选的 `DEEPSEEK_BASE_URL` 继续生效；`DeepSeekHarnessConfig.env` 条目为子进程覆盖它们。`base_url` 与 `api_key` 显式设置这两个变量。

`cwd` 是记录在 SDK 创建会话上的 agent 工作区；`runtime_cwd` 独立选择子进程工作目录。两者在启动前解析为绝对、符号链接已消解的路径。`provider`、`model`、可选的 `reasoning_effort` 与可选正数 `max_tokens` 在初始化时发送。`profile` 默认 `sdk`；`patches` 追加按序 profile 覆盖层。

### 会话与恢复

`harness.resume_session(saved_id)` 从配置的 home 恢复持久历史，不发送输入，并返回可直接 `run()` 的 `Session`。缺失、不可读、不兼容或已被占用的会话会被拒绝。保存原始会话 id，并复用其 home 与工作区。

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

`Session::get_working_directory()` 读取有效目录；`set_working_directory(path)` 修改它并返回校验后的绝对路径。两者都会在不运行模型轮次的情况下创建未知 Session。相对路径基于该 Session 的当前目录解析。origin 元数据、权限与既有进程保持不变。低层客户端提供相同方法，但首参为 `session_id`，并额外提供 `session_wait()`。

### 阻塞门面

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

`DeepSeekHarnessSync` 不得在异步运行时上下文内调用；否则以 `SdkError::NestedRuntime` 失败。

### 显式启动与 patch

`runtime_bin` 选择另一个打包的 `dsh` 可执行程序；SDK 仍会追加 `--profile` 与按序 `--patch` 参数，并要求 Harness home：

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

`launch_args_override` 与 `command`/`args` 是显式适配通道：argv 原样发送，不追加 profile 或 patch，也不要求 `DSH_HOME`。它们用于 fake runtime 或完全自定义的启动器。

## DeepSeekHarness

- `run(input, options)` 拥有一个活动区间：把提示词入队，等它的 id 出现在持久 `agent/inbox/spliced` 回执中，收集通知直到整 agent 下一次 `idle`。`RunResult.final_response` 是该区间最后提交的助手文本，并非归因于该提示词的答案；steering、注入的上下文与其他排队工作都可能在 idle 前参与其中。`finish_reason` 是最后一条 `turn/end` 的 `kind`，没有则为 `None`。
- `start_session(Some(id))` 打开具名会话句柄；`run` 未给会话 id 时创建全新会话。
- `events` 原样携带根会话事件载荷；`notifications` 还包括经 `subagent.started` 血缘边发现的后代通知。子代理有输出时，`subagent.finished` 载荷携带 `lastAssistantMessage`。
- `on_user_question` 安装原生提问处理器；`approval_requests` 开启 server 审批请求。
- `max_tokens` 限制 SDK 创建的 agent 及其进程内后代的模型输出。

## HarnessClient

低层客户端：`start()`/`initialize()`/`session_prompt()`/`resume_session()`/`session_wait()`/`get_working_directory()`/`set_working_directory()`/`request()`/`request_with_timeout()`/`notify()`/`close()`，加上 `subscribe(filter)` 与 `subscribe_session_tree(id)` 通知订阅。`request_with_timeout(method, params, timeout_seconds)` 为单次调用提供 Python SDK 的逐次覆盖：`Some(seconds)` 在该次调用替换配置的 `request_timeout_seconds`，`None` 保持配置值。`next_notification()` 收集没有订阅者匹配的通知。

`initialize()` 有独立的 `initialize_timeout_seconds` 上界（默认 30 秒）；超时消息指出所选 profile 并携带保留的诊断。握手失败会关闭运行时。

`session_prompt()` 在运行时接受消息后立即解析为入队消息 id，绝不等待 agent 活动。发生 panic 的通知过滤器会被隔离在它自己的订阅内，只让该订阅的下一次读取失败。`close()` 执行关闭阶梯：协议 `shutdown` → stdin EOF → SIGTERM → SIGKILL，各带有限等待，然后让挂起请求与订阅失败。已关闭的客户端拒绝复用。

## 客户端审批

只有在处理器已就绪、可与运行并发排空 `next_request()` 时才设置 `approval_requests: true`（或 `InitializeParams::approval_requests`）。决定是一次性的；只有 `allowed-once` 会授权操作。

```rust,no_run
use deepseek_harness_sdk_rs::{ApprovalOutcome, ApprovalRequestParams, ApprovalRequestResult, HarnessClient};

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

## 原生用户提问

把 `on_user_question` 传给 `DeepSeekHarnessConfig` 或 `HarnessClientOptions`，即可通过原生 SDK 协议应答内置 `ask_user_question` 工具。安装处理器会在初始化时声明 `capabilities.userQuestions`。回调收到 `UserQuestionRequest` 与 `UserQuestionCancellation`；返回的 `UserQuestionAnswer` 必须为每个原始问题 id 提供一条答案。SDK 不替调用方选择答案。

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

回调在独立任务上运行，reader 继续处理取消与其他请求，回调也可以继续发起客户端请求。运行时发送 `user-question.cancelled` 或客户端关闭时触发取消；迟到答案会被抑制。回调错误成为 JSON-RPC `-32603`；非法批次与重复 requestId 收到 `-32602`，且不会到达回调。

## 启动通道

最显式者优先：`launch_args_override`、`command`+`args`、`runtime_bin`、`$DSH_RUNTIME_BIN`，最后是下载器。托管通道追加 `--profile <profile>` 与按序 `--patch <绝对路径>`，并要求 `dsh_home` 或非空 `$DSH_HOME`。下载器读取 `$DSH_RUNTIME_PYPI_URL`（索引覆盖，默认 `https://pypi.org/pypi`）、`$DSH_RUNTIME_VERSION`（wheel 版本，默认 crate 版本）与 `$DSH_RUNTIME_CACHE_DIR`（缓存根目录，默认平台缓存目录）。不再有隐式 `cordis.yml` 注入或 `DSH_CORDIS_CONFIG` 默认值。

## 错误

`SdkError` 复现 Python 分类：`JsonRpcResponse`（保留 `code` 与 `data`）、`RequestTimeout`（消息中携带可用的运行时诊断）、`Protocol`（文档化协议违例）、`TransportClosed`（退出码加有界 stderr 尾部）、`Io`、`RuntimeResolve`（下载器失败）、`Config`（缺失 home 或不可用路径）与 `NestedRuntime`（异步上下文内调用同步门面）。

## 开发

```sh
cargo fmt --all
cargo clippy --all-targets --features test-support -- -D warnings
cargo test --features test-support
```

`test-support` feature 构建 `dsh-fake-runtime` 测试二进制，机制层测试经真实管道驱动它；必须带该 feature 运行测试，否则进程级测试会以指引信息失败。无密钥 smoke 层（真实 exe 对脚本化 Messages 模型）在缺少 `DSH_TEST_RUNTIME_EXE` 时自行跳过；CI 会对从 PyPI 下载的运行时 wheel 驱动它。

## 已知限制与暂缓事项

- **无协议版本协商**——握手报告 `serverInfo.version` 但客户端不校验；处于预发布阶段，无兼容承诺。
- **无取消与会话关闭方法**——放弃轮次意味着关闭运行时进程；协议没有提示词取消方法。
- **无提示词级结果归属**——`final_response` 是所收集区间的最后一条助手文本。
- **无 Office 操作**——Python SDK 的 `office` 助手调用单独下载的 Kit CLI；它不属于 wire 协议，本 crate 不暴露。
- **close 后不可重启**——已关闭的 `HarnessClient` 拒绝复用，握手失败后也一样；需要新进程时请新建 harness。
- **下载器耦合 PyPI wheel 流**——构建工作流只保留平台 wheel；布局不同的 wheel 会让解析响亮失败。
