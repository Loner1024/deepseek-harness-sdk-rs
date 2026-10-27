//! The low-level `HarnessClient`: the SDK protocol client underneath the
//! high-level [`crate::api::DeepSeekHarness`] API. It owns the runtime
//! subprocess, the `initialize`/`session_prompt`/`shutdown` requests, the
//! notification subscription fan-out with session-tree scoping, native user
//! questions, and the shutdown ladder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, OnceCell};

use crate::error::SdkError;
use crate::transport::{
    Interceptor, NotificationFilter, NotificationSubscription, Peer, RequestInterceptor,
};

/// One server-to-client notification: the JSON-RPC method plus its payload
/// object. Event payloads pass through verbatim.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Notification {
    /// JSON-RPC method name, e.g. `session.event`.
    pub method: String,
    /// Notification payload object; absent or non-object payloads read as `{}`.
    pub payload: Value,
}

/// One server-to-client request, queued until the caller answers it with
/// [`HarnessClient::respond`] or [`HarnessClient::respond_error`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct IncomingRequest {
    /// Request id, preserved verbatim for the response frame.
    pub id: Value,
    /// JSON-RPC method name.
    pub method: String,
    /// Request params object; absent or non-object params read as `{}`.
    pub payload: Value,
}

/// Parameters for the process-wide `initialize` handshake.
#[derive(Debug, Clone)]
pub struct InitializeParams {
    /// Working directory recorded on every SDK-created session's header.
    pub cwd: PathBuf,
    /// Provider route every SDK-created agent runs on.
    pub provider: String,
    /// Model name every SDK-created agent runs on.
    pub model: String,
    /// Optional adapter-owned reasoning effort for the selected route.
    pub reasoning_effort: Option<String>,
    /// Optional positive output-token cap for SDK-created agents.
    pub max_tokens: Option<u64>,
    /// Opt in to server-to-client `approval/request` calls. Omission leaves
    /// the runtime's composed approval answerers in control.
    pub approval_requests: bool,
}

/// Wire-stable server identity returned by initialization.
#[derive(Debug, Clone)]
pub struct InitializeResult {
    /// `serverInfo` from the handshake response; `None` when the runtime omitted it.
    pub server_info: Option<ServerInfo>,
}

/// The `serverInfo` object of the initialize handshake.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Server identity; the protocol value is `deepseek-harness-sdk-runtime`.
    pub name: Option<String>,
    /// Server version.
    pub version: Option<String>,
}

/// Parameters of a server-to-client `approval/request` payload, after the
/// client opted in with [`InitializeParams::approval_requests`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestParams {
    /// Session owning the operation, including an in-process SDK descendant.
    pub session_id: String,
    /// Tool whose operation needs approval.
    pub tool_name: String,
    /// Exact tool call, when the asker supplied one.
    pub call_id: Option<String>,
    /// Asker-supplied explanation, when available.
    pub reason: Option<String>,
}

/// One-shot approval decision; only [`ApprovalOutcome::AllowedOnce`] grants an
/// operation, and grants never persist beyond it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalRequestResult {
    /// The decision.
    pub outcome: ApprovalOutcome,
}

/// The value of [`ApprovalRequestResult::outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    /// Permit this one operation.
    AllowedOnce,
    /// Refuse the operation.
    Rejected,
    /// Abandon the operation without deciding.
    Cancelled,
}

/// One native `user-question/ask` batch: the runtime's structured question
/// request, with the original question objects preserved verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct UserQuestionRequest {
    /// Correlates cancellation with this request independently of the JSON-RPC id.
    pub request_id: String,
    /// Session whose tool is waiting.
    pub session_id: String,
    /// Original questions, including options, detail, and presentation intent.
    pub questions: Vec<Value>,
}

impl UserQuestionRequest {
    /// Read and validate the wire payload. Questions must retain a string `id`
    /// and `question`; their remaining metadata passes through untouched.
    fn from_payload(payload: &Value) -> Result<Self, ()> {
        let object = payload.as_object().ok_or(())?;
        let request_id = object
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(())?;
        let session_id = object.get("sessionId").and_then(Value::as_str).ok_or(())?;
        let questions = object
            .get("questions")
            .and_then(Value::as_array)
            .filter(|questions| !questions.is_empty())
            .ok_or(())?;
        for question in questions {
            let question = question.as_object().ok_or(())?;
            if question.get("id").and_then(Value::as_str).is_none()
                || question.get("question").and_then(Value::as_str).is_none()
            {
                return Err(());
            }
        }
        Ok(Self {
            request_id: request_id.to_string(),
            session_id: session_id.to_string(),
            questions: questions.clone(),
        })
    }
}

/// Answer to one native question batch; name every original question id once.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UserQuestionAnswer {
    /// One answer per original question.
    pub answers: Vec<UserQuestionAnswerItem>,
}

/// Answer to one original question id, using selected option labels or custom text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UserQuestionAnswerItem {
    /// Original question id.
    pub id: String,
    /// Selected option labels.
    pub selected: Vec<String>,
    /// Optional free text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Cancellation handle passed to a [`UserQuestionHandler`]. Wait on
/// [`Self::cancelled`] or poll [`Self::is_cancelled`] to release host work;
/// a late answer cannot reach the runtime.
#[derive(Clone)]
pub struct UserQuestionCancellation {
    inner: Arc<CancellationInner>,
}

impl UserQuestionCancellation {
    fn new() -> Self {
        Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    /// Whether the runtime cancelled the question or the client closed.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolve when the runtime cancels the question or the client closes.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }
}

impl std::fmt::Debug for UserQuestionCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UserQuestionCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

type UserQuestionFuture = Pin<Box<dyn Future<Output = Result<UserQuestionAnswer, String>> + Send>>;

/// Async callback for native user questions. The SDK advertises
/// `capabilities.userQuestions` when one is installed and runs the callback
/// outside the reader, so it may make further client requests. An `Err`
/// message is returned to the runtime as JSON-RPC error `-32603`.
#[derive(Clone)]
pub struct UserQuestionHandler(
    Arc<dyn Fn(UserQuestionRequest, UserQuestionCancellation) -> UserQuestionFuture + Send + Sync>,
);

impl UserQuestionHandler {
    /// Wrap an async handler; the future must be `Send` and `'static`.
    pub fn new<F, Fut>(handler: F) -> Self
    where
        F: Fn(UserQuestionRequest, UserQuestionCancellation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<UserQuestionAnswer, String>> + Send + 'static,
    {
        Self(Arc::new(move |request, cancellation| {
            Box::pin(handler(request, cancellation))
        }))
    }

    fn call(
        &self,
        request: UserQuestionRequest,
        cancellation: UserQuestionCancellation,
    ) -> UserQuestionFuture {
        (self.0)(request, cancellation)
    }
}

impl std::fmt::Debug for UserQuestionHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UserQuestionHandler(..)")
    }
}

/// How to launch the runtime subprocess and where its artifacts come from.
#[derive(Clone, Debug)]
pub struct HarnessClientOptions {
    /// Explicit launch command; the argv is sent verbatim with `args` and
    /// skips the managed `DSH_HOME` requirement.
    pub command: Option<String>,
    /// Arguments for the explicit `command`.
    pub args: Vec<String>,
    /// Explicit path of the packaged `dsh` executable. The client appends
    /// `--profile` and `--patch` arguments and requires an explicit home.
    pub runtime_bin: Option<PathBuf>,
    /// Explicit argv tuple; wins over every other launch channel, is sent
    /// verbatim without profile or patch arguments, and skips the managed
    /// `DSH_HOME` requirement.
    pub launch_args_override: Option<Vec<String>>,
    /// Harness profile to boot; default `sdk`.
    pub profile: String,
    /// Ordered profile patches, passed as `--patch <absolute path>`.
    pub patches: Vec<PathBuf>,
    /// Explicit Harness home; sets `DSH_HOME`. Managed launches require it
    /// here or as a non-empty `$DSH_HOME`; the SDK never discovers `~/.dsh`.
    pub dsh_home: Option<PathBuf>,
    /// Working directory of the spawned runtime.
    pub cwd: Option<PathBuf>,
    /// Extra environment entries, overlaid on the inherited environment.
    pub env: Option<HashMap<String, String>>,
    /// Bound in seconds on the initial `initialize` handshake. Default `30.0`.
    pub initialize_timeout_seconds: f64,
    /// Request deadline in seconds; `None` waits indefinitely.
    pub request_timeout_seconds: Option<f64>,
    /// Budget in seconds for the protocol `shutdown` request and each wait in
    /// the stdin-EOF → SIGTERM → SIGKILL ladder. Default `1.0`.
    pub shutdown_timeout_seconds: f64,
    /// Handler for native `user-question/ask` requests; installing one
    /// advertises `capabilities.userQuestions` at initialization.
    pub on_user_question: Option<UserQuestionHandler>,
    /// PyPI-style index the downloader reads; default `https://pypi.org/pypi`.
    pub runtime_index_url: Option<String>,
    /// Runtime wheel version to download; default the crate version.
    pub runtime_version: Option<String>,
    /// Downloader cache directory; default the platform cache directory.
    pub runtime_cache_dir: Option<PathBuf>,
}

impl Default for HarnessClientOptions {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            runtime_bin: None,
            launch_args_override: None,
            profile: "sdk".to_string(),
            patches: Vec::new(),
            dsh_home: None,
            cwd: None,
            env: None,
            initialize_timeout_seconds: 30.0,
            request_timeout_seconds: None,
            shutdown_timeout_seconds: 1.0,
            on_user_question: None,
            runtime_index_url: None,
            runtime_version: None,
            runtime_cache_dir: None,
        }
    }
}

/// The resolved launch channel. `managed` launches receive `--profile`,
/// `--patch`, and the required `DSH_HOME`; explicit channels are sent verbatim.
struct ResolvedLaunch {
    command: String,
    args: Vec<String>,
    managed: bool,
}

struct ClientState {
    child: Arc<Mutex<Child>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct QuestionTask {
    cancellation: UserQuestionCancellation,
    handle: tokio::task::JoinHandle<()>,
}

struct ClientInner {
    options: HarnessClientOptions,
    peer: OnceCell<Peer>,
    state: Mutex<Option<ClientState>>,
    parents: Arc<std::sync::Mutex<HashMap<String, String>>>,
    stderr_tail: Arc<std::sync::Mutex<VecDeque<String>>>,
    default_subscription: Mutex<Option<NotificationSubscription>>,
    question_tasks: Arc<std::sync::Mutex<HashMap<String, QuestionTask>>>,
    closing: AtomicBool,
}

/// The protocol client; clone to share one runtime subprocess.
#[derive(Clone)]
pub struct HarnessClient {
    inner: Arc<ClientInner>,
}

/// Bounded stderr tail length, mirroring the Python SDK.
const STDERR_TAIL_LINES: usize = 400;

impl HarnessClient {
    /// Build a client for the given launch options. The runtime subprocess
    /// starts lazily on [`Self::start`].
    pub fn new(options: HarnessClientOptions) -> Self {
        Self {
            inner: Arc::new(ClientInner {
                options,
                peer: OnceCell::new(),
                state: Mutex::new(None),
                parents: Arc::new(std::sync::Mutex::new(HashMap::new())),
                stderr_tail: Arc::new(std::sync::Mutex::new(VecDeque::new())),
                default_subscription: Mutex::new(None),
                question_tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
                closing: AtomicBool::new(false),
            }),
        }
    }

    /// Spawn the runtime subprocess and attach the transport. Idempotent:
    /// a second call on a started client is a no-op.
    pub async fn start(&self) -> Result<(), SdkError> {
        if self.inner.peer.get().is_some() {
            return Ok(());
        }
        self.inner.closing.store(false, Ordering::SeqCst);
        let resolved = self.resolve_launch()?;
        let env = self.build_env(&resolved)?;

        let mut command = Command::new(&resolved.command);
        command
            .args(&resolved.args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.inner.options.cwd {
            // Python resolves the runtime cwd to an absolute, symlink-free
            // path before launch; mirror that here.
            command.current_dir(crate::resolve_path(cwd));
        }
        let mut child = command.spawn().map_err(SdkError::from)?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let peer = Peer::start(
            stdin,
            stdout,
            Some(self.notification_interceptor()),
            Some(Self::request_interceptor(&self.inner)),
            self.inner
                .options
                .request_timeout_seconds
                .map(|seconds| Duration::from_secs_f64(seconds.max(0.0))),
        );
        if self.inner.peer.set(peer.clone()).is_err() {
            // A concurrent start() won the race; reap this duplicate child.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(());
        }
        *self.inner.default_subscription.lock().await = Some(peer.subscribe_default());

        let stderr_eof = Arc::new(tokio::sync::Notify::new());
        let stderr_tail = self.inner.stderr_tail.clone();
        let stderr_eof_task = stderr_eof.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = stderr_tail.lock().unwrap();
                if tail.len() >= STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            stderr_eof_task.notify_one();
        });

        let child = Arc::new(Mutex::new(child));
        let monitor_peer = peer.clone();
        let monitor_child = child.clone();
        let monitor_tail = self.inner.stderr_tail.clone();
        let monitor_questions = self.inner.question_tasks.clone();
        let monitor_task = tokio::spawn(async move {
            monitor_peer.wait_closed().await;
            // Give the stderr task a bounded moment to drain the last lines.
            let _ = tokio::time::timeout(Duration::from_millis(100), stderr_eof.notified()).await;
            let exit_code = monitor_child
                .lock()
                .await
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code());
            for task in monitor_questions.lock().unwrap().values() {
                task.cancellation.cancel();
            }
            let tail: Vec<String> = monitor_tail.lock().unwrap().iter().cloned().collect();
            monitor_peer.fail_closed(SdkError::transport_closed(
                "DeepSeek Harness runtime stdout closed",
                exit_code,
                &tail,
            ));
        });

        *self.inner.state.lock().await = Some(ClientState {
            child,
            tasks: vec![stderr_task, monitor_task],
        });
        Ok(())
    }

    /// Perform the process-wide handshake; a failed handshake closes the
    /// runtime and re-raises, mirroring the Python SDK.
    pub async fn initialize(
        &self,
        params: &InitializeParams,
    ) -> Result<InitializeResult, SdkError> {
        let cwd = crate::resolve_path(&params.cwd);
        let mut payload = Map::new();
        payload.insert(
            "cwd".to_string(),
            Value::from(cwd.to_string_lossy().into_owned()),
        );
        payload.insert("provider".to_string(), Value::from(params.provider.clone()));
        payload.insert("model".to_string(), Value::from(params.model.clone()));
        if let Some(reasoning_effort) = &params.reasoning_effort {
            payload.insert(
                "reasoningEffort".to_string(),
                Value::from(reasoning_effort.clone()),
            );
        }
        if let Some(max_tokens) = params.max_tokens {
            payload.insert("maxTokens".to_string(), Value::from(max_tokens));
        }
        if params.approval_requests {
            payload.insert("approvalRequests".to_string(), Value::Bool(true));
        }
        if self.inner.options.on_user_question.is_some() {
            payload.insert(
                "capabilities".to_string(),
                serde_json::json!({"userQuestions": true}),
            );
        }
        let timeout =
            Duration::from_secs_f64(self.inner.options.initialize_timeout_seconds.max(0.0));
        match self
            .request_with_timeout(
                "initialize",
                Some(Value::Object(payload)),
                Some(timeout.as_secs_f64()),
            )
            .await
        {
            Ok(result) => {
                let server_info = result.get("serverInfo").map(|info| ServerInfo {
                    name: info.get("name").and_then(Value::as_str).map(str::to_string),
                    version: info
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
                Ok(InitializeResult { server_info })
            }
            Err(error) => {
                self.close().await;
                match error {
                    SdkError::RequestTimeout { message } => Err(SdkError::RequestTimeout {
                        message: format!(
                            "{message}\nselected dsh profile {:?}",
                            self.inner.options.profile
                        ),
                    }),
                    SdkError::JsonRpcResponse {
                        code,
                        message,
                        data,
                    } => {
                        let diagnostics = self.runtime_diagnostics().await;
                        let message = if diagnostics.is_empty() {
                            message
                        } else {
                            format!("{message}\n{diagnostics}")
                        };
                        Err(SdkError::JsonRpcResponse {
                            code,
                            message,
                            data,
                        })
                    }
                    other => Err(other),
                }
            }
        }
    }

    /// Enqueue one prompt on one session; resolves to the queued message id
    /// as soon as the runtime accepts it, never waiting for agent activity.
    pub async fn session_prompt(
        &self,
        session_id: &str,
        content_blocks: &[Value],
    ) -> Result<String, SdkError> {
        let payload = serde_json::json!({"sessionId": session_id, "contentBlocks": content_blocks});
        let result = self.request("session/prompt", Some(payload)).await?;
        result
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                SdkError::protocol("session/prompt response requires a string messageId")
            })
    }

    /// Restore a persisted session without enqueueing a prompt; missing,
    /// unreadable, incompatible, or occupied sessions are rejected.
    pub async fn resume_session(&self, session_id: &str) -> Result<(), SdkError> {
        self.request(
            "session/resume",
            Some(serde_json::json!({"sessionId": session_id})),
        )
        .await?;
        Ok(())
    }

    /// Wait for an SDK-owned session and its managed descendants to settle.
    pub async fn session_wait(&self, session_id: &str) -> Result<(), SdkError> {
        self.request(
            "session/wait",
            Some(serde_json::json!({"sessionId": session_id})),
        )
        .await?;
        Ok(())
    }

    /// Read one session's effective working directory.
    pub async fn get_working_directory(&self, session_id: &str) -> Result<String, SdkError> {
        let result = self
            .request(
                "session/working-directory/get",
                Some(serde_json::json!({"sessionId": session_id})),
            )
            .await?;
        result
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                SdkError::protocol("session/working-directory/get response requires a string cwd")
            })
    }

    /// Change one session's effective working directory and return the
    /// validated absolute path. Relative paths use the session's directory.
    pub async fn set_working_directory(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<String, SdkError> {
        let result = self
            .request(
                "session/working-directory/set",
                Some(serde_json::json!({"sessionId": session_id, "path": path})),
            )
            .await?;
        result
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                SdkError::protocol("session/working-directory/set response requires a string cwd")
            })
    }

    /// Send one request frame and await its response, honoring the configured
    /// `request_timeout_seconds`. A timeout carries the runtime diagnostics
    /// (exit code and stderr tail when available), as in the Python SDK.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, SdkError> {
        self.request_with_timeout(method, params, None).await
    }

    /// [`Self::request`] with a per-call deadline in seconds, the Python
    /// SDK's `timeout_seconds` override: `Some(seconds)` replaces the
    /// configured deadline for this call, `None` keeps it.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_seconds: Option<f64>,
    ) -> Result<Value, SdkError> {
        let peer = self.peer()?;
        let result = match timeout_seconds {
            None => peer.request(method, params).await,
            Some(seconds) => {
                peer.request_with_timeout(
                    method,
                    params,
                    Some(Duration::from_secs_f64(seconds.max(0.0))),
                )
                .await
            }
        };
        match result {
            Err(SdkError::RequestTimeout { .. }) => {
                let diagnostics = self.runtime_diagnostics().await;
                let suffix = if diagnostics.is_empty() {
                    String::new()
                } else {
                    format!("\n{diagnostics}")
                };
                Err(SdkError::RequestTimeout {
                    message: format!(
                        "{method} timed out waiting for DeepSeek Harness runtime{suffix}"
                    ),
                })
            }
            other => other,
        }
    }

    /// Subprocess state available for timeout and transport-failure reports:
    /// the exit code when already observed and the bounded stderr tail.
    async fn runtime_diagnostics(&self) -> String {
        let mut parts = Vec::new();
        if let Some(state) = self.inner.state.lock().await.as_ref() {
            let exited = {
                let mut child = state.child.lock().await;
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if let Some(code) = status.code() {
                            parts.push(format!("exit code: {code}"));
                        }
                        true
                    }
                    _ => false,
                }
            };
            if exited {
                // Give the stderr reader a bounded moment to drain the last
                // lines, mirroring the Python SDK's 0.1s join.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let tail: Vec<String> = self
            .inner
            .stderr_tail
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        if !tail.is_empty() {
            parts.push(format!("stderr tail:\n{}", tail.join("\n")));
        }
        parts.join("\n")
    }

    /// Send one notification frame to the runtime.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), SdkError> {
        self.peer()?.notify(method, params)
    }

    /// Wait for the next notification that no subscriber matched.
    pub async fn next_notification(&self) -> Result<Notification, SdkError> {
        let mut guard = self.inner.default_subscription.lock().await;
        let subscription = guard.as_mut().ok_or_else(|| {
            SdkError::transport_closed("DeepSeek Harness runtime is not running", None, &[])
        })?;
        subscription.next().await
    }

    /// Wait for the next server-to-client request queued by the runtime.
    /// Requests handled by [`UserQuestionHandler`] never appear here.
    pub async fn next_request(&self) -> Result<IncomingRequest, SdkError> {
        self.peer()?.next_request().await
    }

    /// Answer a server-to-client request with a result frame.
    pub async fn respond(&self, id: &Value, result: Value) -> Result<(), SdkError> {
        self.peer()?.respond(id, result)
    }

    /// Answer a server-to-client request with an error frame; `data` rides
    /// the error object when present.
    pub async fn respond_error(
        &self,
        id: &Value,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Result<(), SdkError> {
        self.peer()?.respond_error(id, code, message, data)
    }

    /// Subscribe to notifications; `filter` narrows delivery.
    pub fn subscribe(
        &self,
        filter: Option<NotificationFilter>,
    ) -> Result<NotificationSubscription, SdkError> {
        Ok(self.peer()?.subscribe(filter))
    }

    /// Subscribe to one session plus every descendant discovered from
    /// `subagent.started` lineage edges.
    pub fn subscribe_session_tree(
        &self,
        session_id: &str,
    ) -> Result<NotificationSubscription, SdkError> {
        let root = session_id.to_string();
        let parents = self.inner.parents.clone();
        let filter: NotificationFilter = Arc::new(move |notification| {
            let is_descendant = |id: &str| -> bool {
                let parents = parents.lock().unwrap();
                let mut current = id;
                let mut visited = HashSet::new();
                while visited.insert(current) {
                    if current == root {
                        return true;
                    }
                    match parents.get(current) {
                        Some(parent) => current = parent,
                        None => return false,
                    }
                }
                false
            };
            let payload = &notification.payload;
            match notification.method.as_str() {
                "subagent.started" | "subagent.finished" => {
                    if payload
                        .get("parentSessionId")
                        .and_then(Value::as_str)
                        .is_some_and(is_descendant)
                    {
                        return true;
                    }
                    payload.get("childSessionId").and_then(Value::as_str) == Some(root.as_str())
                }
                _ => payload
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(is_descendant),
            }
        });
        self.subscribe(Some(filter))
    }

    /// Close the runtime: protocol `shutdown` → stdin EOF → SIGTERM →
    /// SIGKILL with bounded waits, then fail pending requests and
    /// subscriptions. Idempotent; a closed client rejects further use.
    /// Outstanding user-question handlers receive cancellation first.
    pub async fn close(&self) {
        let question_tasks: Vec<QuestionTask> = {
            let mut tasks = self.inner.question_tasks.lock().unwrap();
            // Publish `closing` under the same lock that guards task
            // insertion, so a question cannot slip in after this drain.
            self.inner.closing.store(true, Ordering::SeqCst);
            tasks.drain().map(|(_, task)| task).collect()
        };
        for task in &question_tasks {
            task.cancellation.cancel();
        }

        let Some(state) = self.inner.state.lock().await.take() else {
            Self::join_question_tasks(question_tasks).await;
            return;
        };
        let peer = match self.inner.peer.get() {
            Some(peer) => peer.clone(),
            None => {
                Self::join_question_tasks(question_tasks).await;
                return;
            }
        };
        let timeout = Duration::from_secs_f64(self.inner.options.shutdown_timeout_seconds.max(0.0));

        if let Err(error) = peer
            .request_with_timeout("shutdown", None, Some(timeout))
            .await
        {
            let mut tail = self.inner.stderr_tail.lock().unwrap();
            tail.push_back(format!("shutdown request failed: {error}"));
        }
        peer.close_stdin();

        let mut child = state.child.lock().await;
        let mut exit_code = None;
        match child.try_wait() {
            Ok(Some(status)) => exit_code = status.code(),
            Ok(None) => {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    // SAFETY: kill(2) with a valid pid and a standard signal
                    // has no precondition beyond the pid being a live child.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                #[cfg(not(unix))]
                let _ = child.start_kill();
                match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(Ok(status)) => exit_code = status.code(),
                    _ => {
                        let _ = child.start_kill();
                        if let Ok(status) = child.wait().await {
                            exit_code = status.code();
                        }
                    }
                }
            }
            Err(_) => {}
        }
        drop(child);

        let tail: Vec<String> = self
            .inner
            .stderr_tail
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        peer.fail_closed(SdkError::transport_closed(
            "DeepSeek Harness runtime closed",
            exit_code,
            &tail,
        ));
        for task in state.tasks {
            task.abort();
        }
        Self::join_question_tasks(question_tasks).await;
    }

    /// Give cooperative handlers a shared 0.5-second window to observe
    /// cancellation; callbacks that ignore it continue detached, as in the
    /// Python SDK, but their late answers are suppressed.
    async fn join_question_tasks(tasks: Vec<QuestionTask>) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        for task in tasks {
            let _ = tokio::time::timeout_at(deadline, task.handle).await;
        }
    }

    fn peer(&self) -> Result<Peer, SdkError> {
        self.inner.peer.get().cloned().ok_or_else(|| {
            SdkError::transport_closed("DeepSeek Harness runtime is not running", None, &[])
        })
    }

    /// Resolve the launch channel, most explicit wins: `launch_args_override`,
    /// `command`+`args`, `runtime_bin`, `$DSH_RUNTIME_BIN`, then the downloader.
    /// Managed channels append `--profile` and ordered `--patch` arguments.
    fn resolve_launch(&self) -> Result<ResolvedLaunch, SdkError> {
        let options = &self.inner.options;
        if let Some(argv) = &options.launch_args_override {
            let mut argv = argv.iter();
            let command = argv.next().ok_or_else(|| SdkError::RuntimeResolve {
                message: "launch_args_override must not be empty".into(),
            })?;
            return Ok(ResolvedLaunch {
                command: command.clone(),
                args: argv.cloned().collect(),
                managed: false,
            });
        }
        if let Some(command) = &options.command {
            return Ok(ResolvedLaunch {
                command: command.clone(),
                args: options.args.clone(),
                managed: false,
            });
        }
        if let Some(bin) = &options.runtime_bin {
            let bin = crate::resolve_path(bin);
            return self.managed_launch(bin.to_string_lossy().into_owned(), Vec::new());
        }
        if let Ok(bin) = std::env::var("DSH_RUNTIME_BIN")
            && !bin.is_empty()
        {
            let bin = crate::resolve_path(Path::new(&bin));
            return self.managed_launch(bin.to_string_lossy().into_owned(), Vec::new());
        }
        #[cfg(feature = "runtime-download")]
        {
            let resolved = crate::runtime::resolve(&crate::runtime::ResolveOptions {
                index_url: options.runtime_index_url.clone(),
                version: options.runtime_version.clone(),
                cache_dir: options.runtime_cache_dir.clone(),
            })?;
            let mut launch_args = resolved.launch_args.into_iter();
            let command = launch_args.next().ok_or_else(|| SdkError::RuntimeResolve {
                message: "the resolved runtime has an empty argv".into(),
            })?;
            self.managed_launch(command, launch_args.collect())
        }
        #[cfg(not(feature = "runtime-download"))]
        {
            Err(SdkError::RuntimeResolve {
                message:
                    "no runtime specified: provide HarnessClientOptions.command/args or runtime_bin \
                      (the runtime-download feature, which resolves a bundled runtime, is disabled)"
                        .into(),
            })
        }
    }

    /// Add the profile selector and ordered patches to a managed launch.
    fn managed_launch(
        &self,
        command: String,
        mut args: Vec<String>,
    ) -> Result<ResolvedLaunch, SdkError> {
        args.push("--profile".to_string());
        args.push(self.inner.options.profile.clone());
        for patch in &self.inner.options.patches {
            let resolved = crate::resolve_path(patch);
            let path = resolved.to_str().ok_or_else(|| SdkError::Config {
                message: format!("patch path is not valid UTF-8: {}", resolved.display()),
            })?;
            args.push("--patch".to_string());
            args.push(path.to_string());
        }
        Ok(ResolvedLaunch {
            command,
            args,
            managed: true,
        })
    }

    /// Assemble the child environment: the inherited environment overlaid
    /// with `options.env`, plus the required `DSH_HOME` for managed launches.
    fn build_env(
        &self,
        resolved: &ResolvedLaunch,
    ) -> Result<HashMap<std::ffi::OsString, std::ffi::OsString>, SdkError> {
        let mut env: HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        if let Some(extra) = &self.inner.options.env {
            for (key, value) in extra {
                env.insert(key.into(), value.into());
            }
        }
        if resolved.managed {
            match &self.inner.options.dsh_home {
                Some(home) => {
                    if home.as_os_str().is_empty() {
                        return Err(SdkError::Config {
                            message: "dsh_home must not be empty".into(),
                        });
                    }
                    let home = crate::resolve_path(home);
                    let home = home.to_str().ok_or_else(|| SdkError::Config {
                        message: format!("dsh_home is not valid UTF-8: {}", home.display()),
                    })?;
                    env.insert("DSH_HOME".into(), home.into());
                }
                None => {
                    let configured = env
                        .get(std::ffi::OsStr::new("DSH_HOME"))
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| !value.trim().is_empty());
                    if !configured {
                        return Err(SdkError::Config {
                            message: "managed launches require an explicit dsh_home or a \
                                      non-empty DSH_HOME; the Rust SDK never discovers ~/.dsh"
                                .into(),
                        });
                    }
                }
            }
        }
        Ok(env)
    }

    /// The reader-task hook recording `subagent.started` lineage and
    /// cancelling a pending native question on `user-question.cancelled`.
    fn notification_interceptor(&self) -> Interceptor {
        let parents = self.inner.parents.clone();
        let question_tasks = self.inner.question_tasks.clone();
        Arc::new(move |notification| {
            if notification.method == "user-question.cancelled"
                && let Some(request_id) = notification
                    .payload
                    .get("requestId")
                    .and_then(Value::as_str)
                && let Some(task) = question_tasks.lock().unwrap().get(request_id)
            {
                task.cancellation.cancel();
            }
            if notification.method != "subagent.started" {
                return;
            }
            let parent = notification
                .payload
                .get("parentSessionId")
                .and_then(Value::as_str);
            let child = notification
                .payload
                .get("childSessionId")
                .and_then(Value::as_str);
            if let (Some(parent), Some(child)) = (parent, child)
                && !parent.is_empty()
                && !child.is_empty()
                && parent != child
            {
                parents
                    .lock()
                    .unwrap()
                    .insert(child.to_string(), parent.to_string());
            }
        })
    }

    /// The reader-task hook dispatching `user-question/ask` to a handler task.
    fn request_interceptor(inner: &Arc<ClientInner>) -> RequestInterceptor {
        let inner = Arc::downgrade(inner);
        Arc::new(move |request: &IncomingRequest| {
            let Some(inner) = inner.upgrade() else {
                return false;
            };
            inner.dispatch_user_question(request)
        })
    }
}

impl ClientInner {
    /// Dispatch one native question without blocking the reader. Returns
    /// whether this hook owns the request. Invalid batches and duplicate
    /// request ids receive `-32602`; handler failures receive `-32603`.
    fn dispatch_user_question(&self, request: &IncomingRequest) -> bool {
        if request.method != "user-question/ask" {
            return false;
        }
        if self.closing.load(Ordering::SeqCst) {
            // The runtime is going away; abandon the question without
            // starting new handler work or answering a dying peer.
            return true;
        }
        let Some(handler) = self.options.on_user_question.clone() else {
            return false;
        };
        let Some(peer) = self.peer.get().cloned() else {
            return false;
        };
        let question = match UserQuestionRequest::from_payload(&request.payload) {
            Ok(question) => question,
            Err(()) => {
                let _ = peer.respond_error(
                    &request.id,
                    -32602,
                    "invalid user-question/ask parameters",
                    None,
                );
                return true;
            }
        };
        let cancellation = UserQuestionCancellation::new();
        let incoming_id = request.id.clone();
        let request_id = question.request_id.clone();
        let task_request_id = request_id.clone();
        let task_question = question;
        let task_cancellation = cancellation.clone();
        let tasks = self.question_tasks.clone();
        let task_peer = peer.clone();
        let mut guard = self.question_tasks.lock().unwrap();
        if self.closing.load(Ordering::SeqCst) {
            return true;
        }
        if guard.contains_key(&request_id) {
            drop(guard);
            let _ = peer.respond_error(
                &request.id,
                -32602,
                "duplicate user-question requestId",
                None,
            );
            return true;
        }
        let handle = tokio::spawn(async move {
            let mut handler_task =
                tokio::spawn(handler.call(task_question, task_cancellation.clone()));
            let outcome = tokio::select! {
                outcome = &mut handler_task => Some(outcome),
                _ = task_cancellation.cancelled() => None,
            };
            if let Some(outcome) = outcome
                && !task_cancellation.is_cancelled()
            {
                match outcome {
                    Ok(Ok(answer)) => match serde_json::to_value(&answer) {
                        Ok(value) => {
                            let _ = task_peer.respond(&incoming_id, value);
                        }
                        Err(error) => {
                            let _ = task_peer.respond_error(
                                &incoming_id,
                                -32603,
                                &format!("failed to encode user-question answer: {error}"),
                                None,
                            );
                        }
                    },
                    Ok(Err(message)) => {
                        let _ = task_peer.respond_error(&incoming_id, -32603, &message, None);
                    }
                    Err(error) if error.is_panic() => {
                        let _ = task_peer.respond_error(
                            &incoming_id,
                            -32603,
                            "user question handler panicked",
                            None,
                        );
                    }
                    Err(error) => {
                        let _ = task_peer.respond_error(
                            &incoming_id,
                            -32603,
                            &format!("user question handler failed: {error}"),
                            None,
                        );
                    }
                }
            }
            let mut guard = tasks.lock().unwrap();
            if guard
                .get(&task_request_id)
                .is_some_and(|task| Arc::ptr_eq(&task.cancellation.inner, &task_cancellation.inner))
            {
                guard.remove(&task_request_id);
            }
        });
        guard.insert(
            request_id,
            QuestionTask {
                cancellation,
                handle,
            },
        );
        true
    }
}
