# Upstream sync report: deepseek-harness-sdk-rs ↔ deepseek-harness

Baseline: official `deepseek-ai/deepseek-harness` master `47f943859b` (pulled, already
up to date) vs. this repository `main`. The comparison reads the official Python SDK
(`python/sdk/src/deepseek_harness/{api,client,errors,models,__init__}.py`), the shared
wire protocol (`packages/sdk/protocol/README.md`, `src/types.ts`, `src/transport.ts`),
and the two SDK agent notes. Official `git log --oneline -20` shows only release /
publication / naming commits touching `python/sdk` or `packages/sdk` after the last
sync; no new wire methods, notification payloads, or client semantics landed.

## Difference inventory

| 项 | 官方行为 | rs 现状（盘点前） | 分类 | 动作 |
|---|---|---|---|---|
| 低层 `request()`/`session_prompt()` 等待响应期间的 drain 参数（`on_notification`/`notification_filter`/`notification_subscription`） | Python 在 0.05s 轮询切片里 drain 订阅，调用方可旁听一次请求期间的匹配通知 | 低层没有这三个参数；高层 `run()` 自己循环 `subscription.next()` 并调用 `on_notification`，区间语义一致 | 有意保留 | 不复刻。异步 Rust 中由调用方显式驱动订阅更清晰；高层 `run()` 行为已与 Python 对齐（此前结论复核成立） |
| `request()` 逐次 `timeout_seconds` 覆盖 | Python `request(..., timeout_seconds=X)` 覆盖配置级 deadline，`None` 保持配置 | 只有配置级 `request_timeout_seconds` | 需同步 | 新增 `HarnessClient::request_with_timeout(method, params, timeout_seconds)`；`request()` 委托 `None`。测试：0.1s 配置下 0.5s 单次覆盖成功、`None` 仍按配置超时 |
| 超时错误携带运行时诊断 | Python（`4445de9921`）把 exit code + stderr tail 附加进 `TimeoutError` | `SdkError::RequestTimeout` 消息只有裸文本 | 需同步 | 客户端在超时路径上补充退出码与 400 行 stderr tail（退出时等 0.1s 排空），格式同 Python。测试断言 stderr 标记出现在超时消息中 |
| `cwd`/`runtime_cwd` 路径解析 | Python `Path.resolve()`：绝对化 + 词法折叠 `.`/`..` + 解析符号链接，再用于子进程 cwd、`DSH_CWD` 与 wire `initialize.cwd` | 只做 `std::path::absolute`，`..` 不折叠、符号链接不消解 | 需同步 | 新增 `crate::resolve_path`（镜像 `Path.resolve(strict=False)`：绝对化、折叠 `.`/`..`（根处钳制）、规范已有前缀、保留尚不存在的尾部），应用于 `DeepSeekHarness::new`、`HarnessClient::start` 与 `initialize`。测试：符号链接工作区经 fake runtime 回读 wire/进程/`DSH_CWD` 三处均为真实路径，另有两组纯函数测试 |
| 通知过滤器抛错 | Python 捕获 predicate 异常：移除该订阅、错误只投给该订阅，reader 与健康订阅继续 | Rust 过滤器 panic 会直接击穿 reader 任务，整个客户端挂起 | 需同步 | `catch_unwind` 隔离过滤器 panic：移除该订阅并向其投递 `SdkError::Protocol`（panic 消息），其他订阅与 unmatched 队列不受影响。测试：broken + healthy 两订阅同场验证 |
| 帧 id 分类 | Python/TS transport 只把 string/number 当 id；`id` 为 null/bool 等且带 `method` 的帧按通知处理 | `id` 为任意 JSON 值且带 `method` 的帧被排队为 server→client 请求 | 需同步 | reader 先判 `valid_id`（string/number），其余带 `method` 帧走通知路径。测试：fake runtime 发 `{"id":null,"method":"tick"}`，客户端收到通知且请求队列为空 |
| 负 `request_timeout_seconds` | Python 的 deadline 立即过期，首个等待切片即超时 | `Duration::from_secs_f64(-x)` panic | 需同步（盘点中发现） | 配置与逐次超时统一钳制为 ≥ 0（立即超时语义）。测试覆盖负配置 |
| `request()` 返回形状 | Python `request()` 要求 `response_model`（pydantic 校验，非 dict 抛 `TypeError`） | rs `request()` 返回原始 `Value`；`initialize`/`session_prompt` 做有类型的结构校验 | 有意保留 | 不复刻 `response_model`。Rust 侧裸 `Value` 是刻意保留的低层扩展，类型化方法提供同等校验；`session_prompt` 缺 `messageId` 映射为 `SdkError::Protocol`（Python 会漏出 pydantic 异常，rs 归口到自身错误分类） |
| `bridge_bin` 启动通道 | `HarnessConfig.bridge_bin`（开发用 bridge 脚本） | 无该字段，有更通用的 `command`+`args` | 有意保留 | `command`+`args` 覆盖任意可执行程序与参数，语义是 `bridge_bin` 的超集 |
| 订阅 API 命名 | `subscribe_notifications(filter)` / `subscribe_session_notifications(id)` | `subscribe(filter)` / `subscribe_session_tree(id)` | 有意保留 | Rust 命名惯例；语义与过滤逻辑逐项一致 |
| 启动前订阅 | Python 允许 start 前建订阅（队列在运行后生效） | start 前 `subscribe` 返回 `TransportClosed` | 有意保留 | 失败快速、不暴露无 peer 状态的订阅；正常用法（start 后订阅 / 高层 run）一致 |
| close 后重启 | Python `close()` 后 `start()` 会再拉起进程（旧队列残留异常，属未设计行为） | close 后客户端永久拒绝复用 | 有意保留 | rs 的单次生命周期更强；官方并未承诺复用已关闭客户端 |
| 默认 `cordis.yml` 注入 | 仅 bundled runtime 解析成功且无非空 `DSH_CORDIS_CONFIG` 时注入；显式 `runtime_bin`/`bridge_bin`/`launch_args_override` 禁用 | 仅下载器解析成功且无非空配置时注入，显式通道禁用 | 已对齐（复核） | 不动作；机制测试已钉住两种路径 |
| 通知/请求队列 | `next_notification`、`next_request`、`respond`/`respond_error`、请求 id 原样回显、subagent 血缘跨订阅保留 | 同名能力均已实现，含血缘 interceptor 与 slot-0 unmatched 队列 | 已对齐（复核） | 不动作；新增 panic 隔离与 id 分类测试加固 |
| 错误分类 | `HarnessError` 下 `JsonRpcError`/`TimeoutError`/`SdkProtocolError`/`TransportClosedError` | `SdkError::{JsonRpcResponse, RequestTimeout, Protocol, TransportClosed}` + `Io`/`RuntimeResolve`/`NestedRuntime` 扩展 | 已对齐（复核） | 不动作；超时诊断同步后 message 语义也一致 |
| 官方新增内容 | `git log --oneline -20` 中 sdk 相关提交：`release(dsh): 0.1.0-rc.5`、`publish the dsh family publicly`、`apply repository naming contract`（均为版本号/命名/发布面改动） | 不涉及客户端行为 | 官方新增需跟进 | 无需要跟进的功能变化；`serverInfo.name = deepseek-harness-sdk-runtime` 协议常量不变，rs 测试继续钉住 |
| 运行时 node 载具（`DSH_RUNTIME_MODE=node`） | 官方仓库内开发验证通道，不进 wheel 发行 | rs 下载器只解析 wheel 内 exe | 有意保留 | wheel 发行物不含 node 闭包；对 PyPI 消费者不构成能力缺口 |

## Implemented

- `HarnessClient::request_with_timeout` — per-call timeout override; `None` keeps the
  configured deadline.
- Timeout diagnostics — `RequestTimeout.message` now carries exit code + bounded
  stderr tail in Python's format.
- `crate::resolve_path` — Python `Path.resolve(strict=False)` semantics for
  `cwd`/`runtime_cwd` at API construction, subprocess launch, and the wire handshake.
- Filter panic containment — a panicking `NotificationFilter` is removed and its
  subscription receives a `Protocol` error; the reader and other subscriptions keep
  working.
- Frame classification — only string/number ids make a frame a request or response;
  other id shapes with a `method` are notifications.
- Negative timeout clamping — immediate timeout instead of a panic.

## Tests

Mechanism tier extended in `sdk/tests/{client,api}.rs` with new
`dsh-fake-runtime` scenarios (`ticks`, `invalid-id-notification`, `cwd`,
`reject-prompt`, slow-request behavior):

- `per_call_timeout_override_replaces_the_config_for_one_request`
- `request_timeout_fires_after_the_deadline` (now asserts stderr diagnostics)
- `negative_timeouts_clamp_to_immediate`
- `panicking_filters_are_contained_to_their_subscription`
- `frames_with_invalid_ids_are_notifications_not_requests`
- `session_prompt_rejects_a_missing_message_id`
- `cwd_and_runtime_cwd_resolve_before_launch_and_handshake` (Unix symlinks)
- `resolve_path_collapses_parent_components_like_python` and
  `resolve_path_clamps_parent_components_at_the_root` (lib unit tests)

Validation: `cargo fmt --all --check`, `cargo clippy --all-targets --features
test-support -- -D warnings`, `cargo test --features test-support` all green
(10 unit + 10 api + 22 client + 6 downloader + smoke + doctests). The change set
does not touch runtime download/zero-config resolution, so the `resolve` example
behavior is unchanged.
