# Upstream sync report: deepseek-harness-sdk-rs ↔ deepseek-harness

Baseline: official `deepseek-ai/deepseek-harness` master `22ddee3792` (2026-10-23),
whose Python SDK and wire protocol describe the `0.2.1-rc.3` development line.
The latest published runtime wheel is `deepseek-harness-runtime-bin 0.2.1rc2`
(2026-10-17), so this repository pins `0.2.1-rc.2`. The rc.3 additions
(approvals and native user questions) are additive initialize fields; the rc2
runtime accepts them, and the mechanism tier covers them with the fake runtime.

The comparison reads the official Python SDK
(`python/sdk/src/deepseek_harness/{api,client,models,errors,office,__init__}.py`),
the shared wire protocol (`packages/sdk/protocol/README.md`, `src/types.ts`),
the TypeScript client (`packages/sdk/client/src/{types,launch}.ts`), and the
runtime wheel reference (`python/sdk-runtime/README.md`).

## Difference inventory

| 项 | 官方行为 | rs 现状（盘点前） | 分类 | 动作 |
|---|---|---|---|---|
| 启动模型 | `dsh --profile <name> [--patch <path>]…`；每次启动要求显式 `dsh_home` 或非空 `DSH_HOME`，绝不发现 `~/.dsh` | 单文件 exe，无 profile/patch，自动注入 wheel 内 `cordis.yml` | 需同步 | 引入 `profile`/`patches`/`dsh_home`；托管启动追加 `--profile`/`--patch` 并校验 `DSH_HOME`；删除 `cordis`/`session_root`/`DSH_CWD` 注入 |
| wheel 布局 | `deepseek-harness-sdk-runtime-<platform>-<arch>`、`-rg` sidecar、macOS `-spawn-helper`、`<platform>/` 资源目录 | 旧 exe 名 `dsh-jsonrpc-agent-pkg-*`，只取 exe/helper/cordis.yml | 需同步 | 提取 `deepseek_harness_runtime/runtime/` 下全部 payload（跳过 repo-only `node/`），保留权限；缓存校验 exe + sidecar + helper |
| 平台映射 | macOS arm64/x64、Linux x64/arm64、Windows x64 | macOS arm64、Linux x64/arm64；缺 x64 mac 与 Windows | 需同步 | 增加 macOS x64 与 Windows x64 wheel 标签；错误消息列出全部支持目标 |
| `initialize` 参数 | `reasoningEffort`、`maxTokens`、`approvalRequests`、`capabilities.userQuestions` | 只有 `maxTokens` | 需同步 | `InitializeParams` 增加 `reasoning_effort`/`approval_requests`；安装提问处理器时自动声明 `capabilities` |
| 初始化超时 | 独立 `initialize_timeout_seconds`（默认 30s）；超时消息带所选 profile；失败关闭运行时 | 只有配置级/逐次请求超时 | 需同步 | 两套配置增加 `initialize_timeout_seconds`；`initialize` 使用独立 deadline，超时追加 `selected dsh profile …`，失败统一 `close()` |
| 原生用户提问 | `user-question/ask` server→client 请求；回调带取消句柄；非法批次/重复 requestId `-32602`，回调失败 `-32603`；取消/关闭抑制迟到答案 | 请求只排队给 `next_request()`，无提问类型或取消 | 需同步 | 新增 `UserQuestionHandler`/`UserQuestionRequest`/`UserQuestionAnswer`/`UserQuestionCancellation`；reader 拦截并分发到独立任务；`user-question.cancelled` 与 `close()` 取消；迟到答案抑制 |
| 客户端审批 | `approval/request` server→client 请求；`approvalRequests: true` 显式开启；一次性决定 `allowed-once`/`rejected`/`cancelled` | 已有通用 `next_request`/`respond`，无类型模型或 initialize 开关 | 需同步 | 新增 `ApprovalRequestParams`/`ApprovalRequestResult`/`ApprovalOutcome`；`InitializeParams.approval_requests` |
| `session/resume` | 从持久 home 恢复会话，不发送输入；缺失/不可读/不兼容/占用都拒绝 | 无 | 需同步 | `HarnessClient::resume_session` + `DeepSeekHarness::resume_session` + sync 门面 |
| `session/wait` | 等待 SDK 会话及其托管后代结算；未知会话拒绝 | 无类型化方法 | 需同步 | `HarnessClient::session_wait` |
| 工作目录 | `session/working-directory/get|set`；读写有效目录，set 返回校验后的绝对路径 | 无 | 需同步 | 低层 get/set；高层 `Session::get_working_directory`/`set_working_directory` |
| `RunResult.session_root` | 官方无此字段；home 由 `dsh_home` 拥有 | 暴露旧的 `DSH_SESSION_ROOT` | 需同步 | 删除字段与配置项 |
| `subagent.finished.lastAssistantMessage` | 子代理最后一条非空助手消息（或累计文本） | 载荷透传，示例已读取 | 已对齐（复核） | 不动作；示例继续渲染该字段 |
| `RunResult.finish_reason` | 最后一条根会话 `turn/end` 的 `data.reason.kind`；缺失 kind 报协议错误 | 已实现 | 已对齐（复核） | 不动作；测试继续钉住 |
| 通知过滤 panic | 只移除该订阅并向其投递错误 | 已实现 | 已对齐（复核） | 不动作 |
| 帧 id 分类 | 只有 string/number 算 id | 已实现 | 已对齐（复核） | 不动作 |
| 逐次请求超时 | `request(..., timeout_seconds=X)` 覆盖配置 | `request_with_timeout` 已实现 | 已对齐（复核） | 不动作 |
| 超时诊断 | exit code + 400 行 stderr tail | 已实现 | 已对齐（复核） | 不动作 |
| `cwd`/`runtime_cwd` 解析 | `Path.resolve()` 语义 | `resolve_path` 已实现 | 已对齐（复核） | 不动作 |
| `env` 语义 | Python 叠加在继承环境上；TS 完整替换 | Rust 叠加，与 Python 一致 | 有意保留 | 不动作 |
| `dsh_bin` 命名 | Python `dsh_bin` | Rust `runtime_bin`（语义相同） | 有意保留 | 不动作 |
| 显式 argv 通道 | Python `_launch_args` 仅内部测试适配器 | Rust `launch_args_override`/`command`+`args` 公开且原样发送 | 有意保留 | 不动作；文档标明显式通道不追加 profile/patch、不要求 `DSH_HOME` |
| close 后重启 | Python close 后可再 `start()` 拉起新进程 | closed client 永久拒绝复用 | 有意保留 | 不复刻；文档明确需要新进程时新建 harness |
| `session_prompt` 等待期间 drain 通知 | Python 在轮询切片里 drain 订阅 | Rust 由调用方显式驱动订阅；高层 `run()` 区间语义一致 | 有意保留 | 不动作 |
| Office 操作 | Python `office.py` 调单独下载的 Kit CLI，不走 wire 协议 | 无 | 有意保留 | 不复刻；Rust crate 不打包/下载 Office 资源 |
| 协议版本协商 | 只报告 `serverInfo.version`（`0.0.1`），不校验 | 一致 | 已对齐（复核） | 不动作 |

## Implemented

- **Profile launch** — `dsh_home`, `profile` (default `sdk`), and ordered
  `patches` on both config layers; managed launches append `--profile` and
  `--patch` and require an explicit home. `DSH_CORDIS_CONFIG`,
  `DSH_SESSION_ROOT`, and `DSH_CWD` injection are gone.
- **Runtime acquisition** — new executable and sidecar names, macOS x64 and
  Windows x64 wheel mappings, full `runtime/` payload extraction (including
  platform resources, excluding the dev-only `node/` carrier), executable
  permissions, and cache validation covering the ripgrep sidecar and macOS
  spawn helper.
- **Handshake** — `reasoningEffort`, `approvalRequests`, and
  `capabilities.userQuestions`; independent initialize deadline naming the
  selected profile on timeout; failed handshakes close the runtime.
- **Native user questions** — typed request/answer/cancellation surface, async
  handler tasks, validation and duplicate handling, cancellation on
  `user-question.cancelled` and `close()`, suppression of late answers, and a
  0.5-second cooperative shutdown window.
- **Approvals** — typed params/result/outcome models and the initialize opt-in;
  the existing `next_request`/`respond` queue carries the request.
- **Sessions** — `resume_session` on the low-level client, the high-level
  harness, and the blocking facade; `session_wait` and typed
  working-directory reads/changes on the low-level client and high-level
  session.
- **Result shape** — `RunResult.session_root` removed.

## Tests

The mechanism tier now covers the new surface through `dsh-fake-runtime`
scenarios (`launch`, `capability`, `approval`, `session`, `slow-initialize`,
`user-question`, `user-question-cancel`, `user-question-invalid`):

- `initialize_advertises_question_capability_and_approval_opt_in`
- `initialize_without_a_handler_does_not_advertise_questions`
- `initialize_timeout_names_the_selected_profile`
- `missing_dsh_home_is_a_config_error`
- `managed_launch_sets_home_profile_and_patches`
- `explicit_launch_sends_argv_verbatim`
- `approval_request_round_trips_one_shot_decisions`
- `user_question_handler_answers_and_preserves_metadata`
- `user_question_handler_failure_becomes_a_protocol_error`
- `user_question_cancellation_reaches_the_handler`
- `invalid_user_question_receives_invalid_params`
- `session_helpers_round_trip`
- `resume_and_working_directory_helpers_round_trip`
- `sync_facade_resumes_a_session`
- `run_carries_native_user_questions_through_the_handler`
- Downloader: new wheel payload extraction, sidecar permissions, `node/`
  exclusion, managed `--profile sdk` + `DSH_HOME`, and no legacy `cordis.yml`
  injection.
- Smoke: one turn through a scripted Messages model, and a persisted-session
  resume through a second runtime process sharing the same `DSH_HOME`.

Validation: `cargo fmt --all --check`, `cargo clippy --all-targets --features
test-support -- -D warnings`, and `cargo test --features test-support` are green
(10 unit + 13 api + 34 client + 5 downloader + 2 smoke + doctest). The keyless
smoke was run locally against the real `0.2.1rc2` executable and passed; the
download path was exercised through `cargo run --example resolve` against PyPI,
which extracted the executable, `-rg` sidecar, and `linux-x64/` resources, and
the same smoke passed against that cache entry.
