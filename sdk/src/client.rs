//! The low-level `HarnessClient`: the SDK protocol client underneath the
//! high-level [`crate::api::DeepSeekHarness`] API. It owns the runtime
//! subprocess, the `initialize`/`session_prompt`/`shutdown` requests, the
//! notification subscription fan-out with session-tree scoping, and the
//! shutdown ladder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, OnceCell};

use crate::error::SdkError;
use crate::transport::{Interceptor, NotificationFilter, NotificationSubscription, Peer};

/// One server-to-client notification: the JSON-RPC method plus its payload
/// object. Event payloads pass through verbatim.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Notification {
    /// JSON-RPC method name, e.g. `session.event`.
    pub method: String,
    /// Notification payload object; absent or non-object payloads read as `{}`.
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
    /// Optional positive output-token cap for SDK-created agents.
    pub max_tokens: Option<u64>,
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

/// How to launch the runtime subprocess and where its artifacts come from.
#[derive(Clone, Debug)]
pub struct HarnessClientOptions {
    /// Explicit launch command; the most explicit channel beside `args`.
    pub command: Option<String>,
    /// Arguments for the explicit `command`.
    pub args: Vec<String>,
    /// Explicit path of the single-file runtime executable.
    pub runtime_bin: Option<PathBuf>,
    /// Explicit argv tuple; wins over every other launch channel.
    pub launch_args_override: Option<Vec<String>>,
    /// Working directory of the spawned runtime.
    pub cwd: Option<PathBuf>,
    /// Extra environment entries, overlaid on the inherited environment.
    pub env: Option<HashMap<String, String>>,
    /// Request deadline in seconds; `None` waits indefinitely.
    pub request_timeout_seconds: Option<f64>,
    /// Budget in seconds for the protocol `shutdown` request and each wait in
    /// the stdin-EOF → SIGTERM → SIGKILL ladder. Default `1.0`.
    pub shutdown_timeout_seconds: f64,
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
            cwd: None,
            env: None,
            request_timeout_seconds: None,
            shutdown_timeout_seconds: 1.0,
            runtime_index_url: None,
            runtime_version: None,
            runtime_cache_dir: None,
        }
    }
}

/// The resolved launch channel plus the default `cordis.yml` when the
/// downloader resolved the runtime.
struct ResolvedLaunch {
    command: String,
    args: Vec<String>,
    default_config: Option<PathBuf>,
}

struct ClientState {
    child: Arc<Mutex<Child>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct ClientInner {
    options: HarnessClientOptions,
    peer: OnceCell<Peer>,
    state: Mutex<Option<ClientState>>,
    parents: Arc<std::sync::Mutex<HashMap<String, String>>>,
    stderr_tail: Arc<std::sync::Mutex<VecDeque<String>>>,
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
            }),
        }
    }

    /// Spawn the runtime subprocess and attach the transport. Idempotent:
    /// a second call on a started client is a no-op.
    pub async fn start(&self) -> Result<(), SdkError> {
        if self.inner.peer.initialized() {
            return Ok(());
        }
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
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(SdkError::from)?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let peer = Peer::start(
            stdin,
            stdout,
            Some(self.lineage_interceptor()),
            self.inner
                .options
                .request_timeout_seconds
                .map(Duration::from_secs_f64),
        );
        if self.inner.peer.set(peer.clone()).is_err() {
            // A concurrent start() won the race; reap this duplicate child.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(());
        }

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
            stderr_eof_task.notify_waiters();
        });

        let child = Arc::new(Mutex::new(child));
        let monitor_peer = peer.clone();
        let monitor_child = child.clone();
        let monitor_tail = self.inner.stderr_tail.clone();
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
        let cwd = std::path::absolute(&params.cwd)?;
        let mut payload = Map::new();
        payload.insert(
            "cwd".to_string(),
            Value::from(cwd.to_string_lossy().into_owned()),
        );
        payload.insert("provider".to_string(), Value::from(params.provider.clone()));
        payload.insert("model".to_string(), Value::from(params.model.clone()));
        if let Some(max_tokens) = params.max_tokens {
            payload.insert("maxTokens".to_string(), Value::from(max_tokens));
        }
        match self
            .request("initialize", Some(Value::Object(payload)))
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
                Err(error)
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

    /// Send one request frame and await its response.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, SdkError> {
        let peer = self.inner.peer.get().ok_or_else(|| {
            SdkError::transport_closed("DeepSeek Harness runtime is not running", None, &[])
        })?;
        peer.request(method, params).await
    }

    /// Send one notification frame to the runtime.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), SdkError> {
        let peer = self.inner.peer.get().ok_or_else(|| {
            SdkError::transport_closed("DeepSeek Harness runtime is not running", None, &[])
        })?;
        peer.notify(method, params)
    }

    /// Subscribe to notifications; `filter` narrows delivery.
    pub fn subscribe(
        &self,
        filter: Option<NotificationFilter>,
    ) -> Result<NotificationSubscription, SdkError> {
        let peer = self.inner.peer.get().ok_or_else(|| {
            SdkError::transport_closed("DeepSeek Harness runtime is not running", None, &[])
        })?;
        Ok(peer.subscribe(filter))
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
    pub async fn close(&self) {
        let Some(state) = self.inner.state.lock().await.take() else {
            return;
        };
        let peer = match self.inner.peer.get() {
            Some(peer) => peer,
            None => return,
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
    }

    /// Resolve the launch channel, most explicit wins: `launch_args_override`,
    /// `command`+`args`, `runtime_bin`, `$DSH_RUNTIME_BIN`, then the downloader.
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
                default_config: None,
            });
        }
        if let Some(command) = &options.command {
            return Ok(ResolvedLaunch {
                command: command.clone(),
                args: options.args.clone(),
                default_config: None,
            });
        }
        if let Some(bin) = &options.runtime_bin {
            return Ok(ResolvedLaunch {
                command: bin.to_string_lossy().into_owned(),
                args: Vec::new(),
                default_config: None,
            });
        }
        if let Ok(bin) = std::env::var("DSH_RUNTIME_BIN")
            && !bin.is_empty()
        {
            return Ok(ResolvedLaunch {
                command: bin,
                args: Vec::new(),
                default_config: None,
            });
        }
        #[cfg(feature = "runtime-download")]
        {
            let resolved = crate::runtime::resolve(&crate::runtime::ResolveOptions {
                index_url: options.runtime_index_url.clone(),
                version: options.runtime_version.clone(),
                cache_dir: options.runtime_cache_dir.clone(),
            })?;
            Ok(ResolvedLaunch {
                command: resolved.launch_args[0].clone(),
                args: resolved.launch_args[1..].to_vec(),
                default_config: Some(resolved.default_config),
            })
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

    /// Assemble the child environment: the inherited environment overlaid
    /// with `options.env`, then the default `cordis.yml` injected only when
    /// the downloader resolved the runtime and no non-empty config exists.
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
        if let Some(config) = &resolved.default_config {
            let has_config = env
                .get(std::ffi::OsStr::new("DSH_CORDIS_CONFIG"))
                .and_then(|value| value.to_str())
                .is_some_and(|value| !value.is_empty());
            if !has_config {
                env.insert(
                    "DSH_CORDIS_CONFIG".into(),
                    config.to_string_lossy().into_owned().into(),
                );
            }
        }
        Ok(env)
    }

    /// The reader-task hook recording `subagent.started` lineage before fan-out.
    fn lineage_interceptor(&self) -> Interceptor {
        let parents = self.inner.parents.clone();
        Arc::new(move |notification| {
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
}
