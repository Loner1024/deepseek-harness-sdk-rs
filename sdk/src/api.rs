//! The high-level own-runtime API: [`DeepSeekHarness`], [`Session`], and
//! [`run`](DeepSeekHarness::run) — the design twin of the Python SDK's
//! `DeepSeekHarness` / `Session` / `run`, with an async core and a thin
//! blocking facade ([`DeepSeekHarnessSync`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::client::{
    HarnessClient, HarnessClientOptions, InitializeParams, Notification, UserQuestionHandler,
};
use crate::error::SdkError;

/// Configuration for launching the local `DeepSeek` Harness SDK runtime.
///
/// The runtime inherits the caller's environment by default, so existing
/// `DEEPSEEK_API_KEY` and `DEEPSEEK_BASE_URL` settings keep working; `env`
/// entries overlay it for the subprocess. Managed launches require an explicit
/// Harness home through `dsh_home` or a non-empty `DSH_HOME`; the SDK never
/// discovers `~/.dsh` implicitly. Explicit `launch_args_override` callers own
/// their argv and environment.
#[derive(Clone, Debug)]
pub struct DeepSeekHarnessConfig {
    /// Provider route for the `initialize` handshake. Default `deepseek-official`.
    pub provider: String,
    /// Model name for the `initialize` handshake. Default `deepseek-v4-flash`.
    pub model: String,
    /// Optional adapter-owned reasoning effort for the selected route.
    pub reasoning_effort: Option<String>,
    /// Optional positive output-token cap inherited by SDK-created agents.
    pub max_tokens: Option<u64>,
    /// Workspace directory recorded on every SDK-created session; resolved to
    /// an absolute, symlink-free path. Default the process working directory.
    pub cwd: Option<PathBuf>,
    /// Working directory of the spawned runtime; resolved the same way.
    /// Default the workspace `cwd`.
    pub runtime_cwd: Option<PathBuf>,
    /// Explicit Harness home; sets `DSH_HOME`. Managed launches require it
    /// unless the child environment already provides a non-empty `DSH_HOME`.
    pub dsh_home: Option<PathBuf>,
    /// Harness profile to boot. Default `sdk`.
    pub profile: String,
    /// Ordered profile patches, applied after the profile layer.
    pub patches: Vec<PathBuf>,
    /// Extra environment entries for the runtime subprocess.
    pub env: HashMap<String, String>,
    /// Explicit path of the packaged `dsh` executable. The SDK appends the
    /// profile and patch arguments.
    pub runtime_bin: Option<PathBuf>,
    /// Explicit argv tuple; wins over every other launch channel, is sent
    /// verbatim without profile or patch arguments, and skips the managed
    /// `DSH_HOME` requirement.
    pub launch_args_override: Option<Vec<String>>,
    /// Bound in seconds on the initial `initialize` handshake. Default `30.0`.
    pub initialize_timeout_seconds: f64,
    /// Request deadline in seconds; `None` waits indefinitely.
    pub request_timeout_seconds: Option<f64>,
    /// Budget in seconds for the protocol `shutdown` request and each wait in
    /// the shutdown ladder. Default `1.0`.
    pub shutdown_timeout_seconds: f64,
    /// Provider base URL; sets `DEEPSEEK_BASE_URL` when given.
    pub base_url: Option<String>,
    /// Provider API key; sets `DEEPSEEK_API_KEY` when given.
    pub api_key: Option<String>,
    /// Handler for native `user-question/ask` requests; installing one
    /// advertises `capabilities.userQuestions` at initialization.
    pub on_user_question: Option<UserQuestionHandler>,
    /// Opt in to server-to-client `approval/request` calls. When enabled,
    /// drain [`HarnessClient::next_request`] concurrently with a run and
    /// answer through [`HarnessClient::respond`].
    pub approval_requests: bool,
    /// PyPI-style index the downloader reads; default `https://pypi.org/pypi`.
    pub runtime_index_url: Option<String>,
    /// Runtime wheel version to download; default the crate version.
    pub runtime_version: Option<String>,
    /// Downloader cache directory; default the platform cache directory.
    pub runtime_cache_dir: Option<PathBuf>,
}

impl Default for DeepSeekHarnessConfig {
    fn default() -> Self {
        Self {
            provider: "deepseek-official".to_string(),
            model: "deepseek-v4-flash".to_string(),
            reasoning_effort: None,
            max_tokens: None,
            cwd: None,
            runtime_cwd: None,
            dsh_home: None,
            profile: "sdk".to_string(),
            patches: Vec::new(),
            env: HashMap::new(),
            runtime_bin: None,
            launch_args_override: None,
            initialize_timeout_seconds: 30.0,
            request_timeout_seconds: None,
            shutdown_timeout_seconds: 1.0,
            base_url: None,
            api_key: None,
            on_user_question: None,
            approval_requests: false,
            runtime_index_url: None,
            runtime_version: None,
            runtime_cache_dir: None,
        }
    }
}

/// Options for one [`DeepSeekHarness::run`] call.
#[derive(Default)]
pub struct RunOptions<'a> {
    /// Session to prompt; default a fresh session id.
    pub session_id: Option<&'a str>,
    /// Optional callback invoked for every collected notification.
    pub on_notification: Option<&'a (dyn Fn(&Notification) + Send + Sync)>,
}

/// The result of one owned run interval.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Session the prompt ran on.
    pub session_id: String,
    /// Last committed assistant text of the root session within the interval.
    pub final_response: String,
    /// Reason kind of the last `turn/end` in the interval, if any.
    pub finish_reason: Option<String>,
    /// Root-session events collected during the interval, in protocol order.
    pub events: Vec<Value>,
    /// All collected notifications, in protocol order.
    pub notifications: Vec<Notification>,
}

/// Prompt input: plain text becomes one text content block; a block list is
/// sent verbatim as the user message.
#[derive(Debug, Clone)]
pub enum Input {
    /// One text content block.
    Text(String),
    /// Content blocks sent verbatim.
    Blocks(Vec<Value>),
}

impl Input {
    fn into_blocks(self) -> Vec<Value> {
        match self {
            Self::Text(text) => vec![serde_json::json!({"type": "text", "text": text})],
            Self::Blocks(blocks) => blocks,
        }
    }
}

impl From<&str> for Input {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

impl From<String> for Input {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<Vec<Value>> for Input {
    fn from(blocks: Vec<Value>) -> Self {
        Self::Blocks(blocks)
    }
}

struct HarnessInner {
    config: DeepSeekHarnessConfig,
    client: HarnessClient,
    initialized: std::sync::Mutex<bool>,
    /// The resolved workspace cwd carried on the initialize handshake.
    cwd: PathBuf,
}

/// Reusable harness: the runtime subprocess starts lazily on the first
/// [`run`](Self::run) and stays owned by this instance across calls. Call
/// [`close`](Self::close) when finished; if the last handle is dropped
/// without closing, `kill_on_drop` terminates the child.
#[derive(Clone)]
pub struct DeepSeekHarness {
    inner: Arc<HarnessInner>,
}

impl DeepSeekHarness {
    /// Build a harness for the given configuration.
    pub fn new(config: DeepSeekHarnessConfig) -> Self {
        // Python's `Path.resolve()` semantics: absolute and symlink-resolved,
        // with any not-yet-existing tail preserved. Applied before environment
        // injection and the wire handshake, and again at subprocess launch.
        let cwd = config.cwd.as_deref().map_or_else(
            || {
                std::env::current_dir()
                    .map_or_else(|_| PathBuf::from("."), |path| crate::resolve_path(&path))
            },
            crate::resolve_path,
        );
        let runtime_cwd = config
            .runtime_cwd
            .as_deref()
            .map_or_else(|| cwd.clone(), crate::resolve_path);
        let mut env = config.env.clone();
        if let Some(base_url) = &config.base_url {
            env.insert("DEEPSEEK_BASE_URL".to_string(), base_url.clone());
        }
        if let Some(api_key) = &config.api_key {
            env.insert("DEEPSEEK_API_KEY".to_string(), api_key.clone());
        }
        let client = HarnessClient::new(HarnessClientOptions {
            runtime_bin: config.runtime_bin.clone(),
            launch_args_override: config.launch_args_override.clone(),
            profile: config.profile.clone(),
            patches: config.patches.clone(),
            dsh_home: config.dsh_home.clone(),
            cwd: Some(runtime_cwd),
            env: Some(env),
            initialize_timeout_seconds: config.initialize_timeout_seconds,
            request_timeout_seconds: config.request_timeout_seconds,
            shutdown_timeout_seconds: config.shutdown_timeout_seconds,
            on_user_question: config.on_user_question.clone(),
            runtime_index_url: config.runtime_index_url.clone(),
            runtime_version: config.runtime_version.clone(),
            runtime_cache_dir: config.runtime_cache_dir.clone(),
            ..Default::default()
        });
        Self {
            inner: Arc::new(HarnessInner {
                config,
                client,
                initialized: std::sync::Mutex::new(false),
                cwd,
            }),
        }
    }

    /// Start the runtime and perform the memoized `initialize` handshake.
    pub async fn start(&self) -> Result<(), SdkError> {
        if *self.inner.initialized.lock().unwrap() {
            return Ok(());
        }
        self.inner.client.start().await?;
        self.inner
            .client
            .initialize(&InitializeParams {
                cwd: self.inner.cwd.clone(),
                provider: self.inner.config.provider.clone(),
                model: self.inner.config.model.clone(),
                reasoning_effort: self.inner.config.reasoning_effort.clone(),
                max_tokens: self.inner.config.max_tokens,
                approval_requests: self.inner.config.approval_requests,
            })
            .await?;
        *self.inner.initialized.lock().unwrap() = true;
        Ok(())
    }

    /// Close the runtime with the shutdown ladder and reset the client.
    pub async fn close(&self) {
        self.inner.client.close().await;
        *self.inner.initialized.lock().unwrap() = false;
    }

    /// Open a named session handle, or a fresh session when `session_id` is `None`.
    pub async fn start_session(&self, session_id: Option<&str>) -> Result<Session, SdkError> {
        self.start().await?;
        let id = match session_id {
            Some(id) => id.to_string(),
            None => format!("session-{}", Uuid::new_v4().simple()),
        };
        Ok(Session {
            harness: self.clone(),
            id,
        })
    }

    /// Restore a persisted session from the configured Harness home without
    /// sending input, and return a handle ready for [`Session::run`]. Missing,
    /// unreadable, incompatible, or occupied sessions are rejected.
    pub async fn resume_session(&self, session_id: &str) -> Result<Session, SdkError> {
        self.start().await?;
        self.inner.client.resume_session(session_id).await?;
        Ok(Session {
            harness: self.clone(),
            id: session_id.to_string(),
        })
    }

    /// Run one turn: enqueue the prompt, wait for its inbox receipt, collect
    /// notifications until the agent-wide next `idle`, and return the run
    /// result. `final_response` is the last committed assistant text of the
    /// interval, not a prompt-attributed answer.
    pub async fn run(
        &self,
        input: impl Into<Input>,
        options: &RunOptions<'_>,
    ) -> Result<RunResult, SdkError> {
        self.start().await?;
        let session_id = match options.session_id {
            Some(id) => id.to_string(),
            None => format!("session-{}", Uuid::new_v4().simple()),
        };
        let blocks = input.into().into_blocks();
        let mut subscription = self.inner.client.subscribe_session_tree(&session_id)?;
        let message_id = self
            .inner
            .client
            .session_prompt(&session_id, &blocks)
            .await?;

        let mut notifications = Vec::new();
        let mut events = Vec::new();
        let mut received = false;
        loop {
            let notification = subscription.next().await?;
            if !received {
                if !crate::events::is_inbox_receipt(&notification, &session_id, &message_id) {
                    continue;
                }
                received = true;
            }
            if let Some(callback) = options.on_notification {
                callback(&notification);
            }
            if notification.method == "session.event"
                && notification
                    .payload
                    .get("sessionId")
                    .and_then(Value::as_str)
                    == Some(session_id.as_str())
                && let Some(event) = notification
                    .payload
                    .get("event")
                    .filter(|event| event.is_object())
            {
                events.push(event.clone());
            }
            notifications.push(notification.clone());
            if notification.method == "session.status"
                && notification
                    .payload
                    .get("sessionId")
                    .and_then(Value::as_str)
                    == Some(session_id.as_str())
                && notification.payload.get("status").and_then(Value::as_str) == Some("idle")
            {
                break;
            }
        }

        Ok(RunResult {
            session_id,
            final_response: crate::events::final_response(&events),
            finish_reason: crate::events::finish_reason(&events)?,
            events,
            notifications,
        })
    }

    /// The underlying protocol client, for callers that need the low-level surface.
    #[must_use]
    pub fn client(&self) -> &HarnessClient {
        &self.inner.client
    }

    /// The configuration this harness was built with.
    #[must_use]
    pub fn config(&self) -> &DeepSeekHarnessConfig {
        &self.inner.config
    }
}

/// A named session handle owned by one harness.
#[derive(Clone)]
pub struct Session {
    harness: DeepSeekHarness,
    id: String,
}

impl Session {
    /// The session id prompts run on.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Run one turn on this session.
    pub async fn run(
        &self,
        input: impl Into<Input>,
        on_notification: Option<&(dyn Fn(&Notification) + Send + Sync)>,
    ) -> Result<RunResult, SdkError> {
        self.harness
            .run(
                input,
                &RunOptions {
                    session_id: Some(&self.id),
                    on_notification,
                },
            )
            .await
    }

    /// Read the effective working directory, recovering a missing directory
    /// to the session origin.
    pub async fn get_working_directory(&self) -> Result<String, SdkError> {
        self.harness.start().await?;
        self.harness
            .inner
            .client
            .get_working_directory(&self.id)
            .await
    }

    /// Change the effective working directory while existing processes keep
    /// their own directories; returns the validated absolute path.
    pub async fn set_working_directory(&self, path: impl AsRef<Path>) -> Result<String, SdkError> {
        let path = path.as_ref().to_str().ok_or_else(|| SdkError::Config {
            message: format!(
                "working directory is not valid UTF-8: {}",
                path.as_ref().display()
            ),
        })?;
        self.harness.start().await?;
        self.harness
            .inner
            .client
            .set_working_directory(&self.id, path)
            .await
    }
}

/// Blocking facade over [`DeepSeekHarness`]: one current-thread tokio runtime
/// behind synchronous methods. Must not be used from inside an async runtime
/// context; doing so fails with [`SdkError::NestedRuntime`].
pub struct DeepSeekHarnessSync {
    runtime: Option<tokio::runtime::Runtime>,
    harness: DeepSeekHarness,
}

impl DeepSeekHarnessSync {
    /// Build the blocking facade around a fresh harness.
    pub fn new(config: DeepSeekHarnessConfig) -> Result<Self, SdkError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
            harness: DeepSeekHarness::new(config),
        })
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime
            .as_ref()
            .expect("sync facade runtime is present until drop")
    }

    /// Start the runtime and perform the `initialize` handshake.
    pub fn start(&self) -> Result<(), SdkError> {
        Self::guard()?;
        self.runtime().block_on(self.harness.start())
    }

    /// Open a named session handle, or a fresh session when `session_id` is `None`.
    pub fn start_session(&self, session_id: Option<&str>) -> Result<Session, SdkError> {
        Self::guard()?;
        self.runtime()
            .block_on(self.harness.start_session(session_id))
    }

    /// Restore a persisted session and return a handle ready to run.
    pub fn resume_session(&self, session_id: &str) -> Result<Session, SdkError> {
        Self::guard()?;
        self.runtime()
            .block_on(self.harness.resume_session(session_id))
    }

    /// Run one turn and block until the interval settles.
    pub fn run(
        &self,
        input: impl Into<Input>,
        options: &RunOptions<'_>,
    ) -> Result<RunResult, SdkError> {
        Self::guard()?;
        self.runtime().block_on(self.harness.run(input, options))
    }

    /// Close the runtime with the shutdown ladder.
    pub fn close(&self) -> Result<(), SdkError> {
        Self::guard()?;
        self.runtime().block_on(self.harness.close());
        Ok(())
    }

    /// The underlying async harness.
    pub fn harness(&self) -> &DeepSeekHarness {
        &self.harness
    }

    fn guard() -> Result<(), SdkError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(SdkError::NestedRuntime);
        }
        Ok(())
    }
}

impl Drop for DeepSeekHarnessSync {
    fn drop(&mut self) {
        // Best effort: run the shutdown ladder when the drop happens outside
        // an async context; inside one, move the runtime to a detached thread
        // so neither the close nor the runtime drop blocks an async caller.
        // `kill_on_drop` on the child also guarantees the subprocess dies
        // with the client.
        let harness = self.harness.clone();
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::spawn(move || {
                runtime.block_on(harness.close());
            });
        } else {
            runtime.block_on(harness.close());
        }
    }
}
