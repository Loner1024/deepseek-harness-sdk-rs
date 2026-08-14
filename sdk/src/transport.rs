//! Line-framed JSON-RPC 2.0 over the runtime child's stdio.
//!
//! One compact JSON frame per line: `id`+`method` frames are requests, `id`
//! frames are responses, `method` frames are notifications. Illegal JSON lines
//! are ignored. Server-to-client requests queue for the caller to answer with
//! [`Peer::respond`]/[`Peer::respond_error`], mirroring the Python SDK's
//! reserved approval-flow surface.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};

use crate::client::{IncomingRequest, Notification};
use crate::error::SdkError;

/// Runs before each notification is fanned out to subscribers; the client
/// uses it to record `subagent.started` lineage so session-tree filters see
/// the updated relationships for the very notification that carries them.
pub(crate) type Interceptor = Arc<dyn Fn(&Notification) + Send + Sync>;

/// Per-subscription predicate, evaluated by the reader task.
pub type NotificationFilter = Arc<dyn Fn(&Notification) -> bool + Send + Sync>;

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, SdkError>>>>>;

/// One item the writer task carries to the runtime's stdin.
enum OutFrame {
    /// A complete JSON-RPC frame (one line, no trailing newline).
    Line(String),
    /// Close stdin (EOF) and exit the writer task.
    CloseStdin,
}

/// One delivery to a subscriber: a notification or the terminal error pushed
/// when the runtime closes, mirroring the Python SDK's per-subscriber queues.
enum SubMsg {
    Notification(Notification),
    Closed(SdkError),
}

/// One delivery on the incoming-request queue: a server-to-client request or
/// the terminal error pushed when the runtime closes.
enum IncomingMsg {
    Request(IncomingRequest),
    Closed(SdkError),
}

/// Shared state between the reader task and [`Peer`] handles.
pub(crate) struct PeerShared {
    /// Registered subscribers; the reader task evaluates their filters.
    /// Slot `0` is the unmatched-notification queue (`next_notification`).
    subscribers: std::sync::Mutex<HashMap<u64, SubscriberSlot>>,
    /// Server-to-client requests, queued for `next_request`.
    requests: mpsc::UnboundedSender<IncomingMsg>,
    /// First terminal error; set once by [`Peer::fail_closed`].
    closed_err: std::sync::Mutex<Option<SdkError>>,
    /// Fired when the reader hits EOF or a write fails; the client's monitor
    /// task resolves the rich [`SdkError::TransportClosed`] from it.
    pub(crate) eof: Notify,
}

struct SubscriberSlot {
    tx: mpsc::UnboundedSender<SubMsg>,
    filter: Option<NotificationFilter>,
}

/// The client half of the transport: request correlation, notification
/// subscriptions, the incoming-request queue, and stdin lifecycle.
#[derive(Clone)]
pub(crate) struct Peer {
    pending: Pending,
    out_tx: mpsc::UnboundedSender<OutFrame>,
    shared: Arc<PeerShared>,
    request_timeout: Option<Duration>,
    request_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<IncomingMsg>>>>,
}

impl Peer {
    /// Attach the transport to the runtime's stdin/stdout and start the
    /// reader and writer tasks.
    pub(crate) fn start<R, W>(
        mut stdin: W,
        stdout: R,
        intercept: Option<Interceptor>,
        request_timeout: Option<Duration>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (requests_tx, requests_rx) = mpsc::unbounded_channel::<IncomingMsg>();
        let shared = Arc::new(PeerShared {
            subscribers: {
                let mut map = HashMap::new();
                let (tx, _rx) = mpsc::unbounded_channel::<SubMsg>();
                map.insert(0u64, SubscriberSlot { tx, filter: None });
                std::sync::Mutex::new(map)
            },
            requests: requests_tx,
            closed_err: std::sync::Mutex::new(None),
            eof: Notify::new(),
        });
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutFrame>();

        let reader_pending = pending.clone();
        let reader_shared = shared.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    // EOF or a read error both mean the runtime is gone.
                    Ok(None) | Err(_) => break,
                };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(object) = message.as_object() else {
                    continue;
                };
                let id = object.get("id");
                let method = object.get("method").and_then(Value::as_str);
                match (id, method) {
                    (Some(raw_id), Some(method)) => {
                        let payload = object
                            .get("params")
                            .filter(|p| p.is_object())
                            .cloned()
                            .unwrap_or_else(|| Value::Object(Map::new()));
                        let request = IncomingRequest {
                            id: raw_id.clone(),
                            method: method.to_string(),
                            payload,
                        };
                        let _ = reader_shared.requests.send(IncomingMsg::Request(request));
                    }
                    (Some(id), None) => {
                        let id = id_key(id);
                        let Some(sender) = reader_pending.lock().await.remove(&id) else {
                            continue;
                        };
                        if let Some(error) = object.get("error") {
                            let code = error.get("code").and_then(Value::as_i64);
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("JSON-RPC error")
                                .to_string();
                            let data = error.get("data").cloned();
                            let _ = sender.send(Err(SdkError::JsonRpcResponse {
                                code,
                                message,
                                data,
                            }));
                        } else {
                            let _ = sender
                                .send(Ok(object.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                    (None, Some(method)) => {
                        let payload = object
                            .get("params")
                            .filter(|p| p.is_object())
                            .cloned()
                            .unwrap_or_else(|| Value::Object(Map::new()));
                        let notification = Notification {
                            method: method.to_string(),
                            payload,
                        };
                        if let Some(hook) = &intercept {
                            hook(&notification);
                        }
                        let subscribers = reader_shared.subscribers.lock().unwrap();
                        let mut delivered = false;
                        for (slot_id, slot) in subscribers.iter() {
                            if *slot_id == 0 {
                                continue;
                            }
                            let matches = slot.filter.as_ref().is_none_or(|f| f(&notification));
                            if matches {
                                let _ = slot.tx.send(SubMsg::Notification(notification.clone()));
                                delivered = true;
                            }
                        }
                        // Notifications no subscriber matched land on the
                        // unmatched queue, mirroring the Python SDK.
                        if !delivered && let Some(slot) = subscribers.get(&0) {
                            let _ = slot.tx.send(SubMsg::Notification(notification));
                        }
                    }
                    (None, None) => continue,
                }
            }
            reader_shared.eof.notify_waiters();
        });

        let writer_shared = shared.clone();
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                match frame {
                    OutFrame::Line(line) => {
                        let mut frame = line.into_bytes();
                        frame.push(b'\n');
                        if stdin_write(&mut stdin, &frame).await.is_err() {
                            writer_shared.eof.notify_waiters();
                            return;
                        }
                    }
                    OutFrame::CloseStdin => return,
                }
            }
        });

        Self {
            pending,
            out_tx,
            shared,
            request_timeout,
            request_rx: Arc::new(Mutex::new(Some(requests_rx))),
        }
    }

    /// Wait for the next server-to-client request queued by the reader task.
    pub(crate) async fn next_request(&self) -> Result<IncomingRequest, SdkError> {
        let mut guard = self.request_rx.lock().await;
        let receiver = guard
            .as_mut()
            .expect("the incoming-request receiver lives for the peer's lifetime");
        match receiver.recv().await {
            Some(IncomingMsg::Request(request)) => Ok(request),
            Some(IncomingMsg::Closed(error)) => Err(error),
            None => Err(self.closed_error().unwrap_or_else(|| {
                SdkError::transport_closed("DeepSeek Harness runtime closed", None, &[])
            })),
        }
    }

    /// Answer a server-to-client request with a result frame.
    pub(crate) fn respond(&self, id: &Value, result: Value) -> Result<(), SdkError> {
        if let Some(err) = self.closed_error() {
            return Err(err);
        }
        let frame = Map::from_iter([
            ("jsonrpc".to_string(), Value::from("2.0")),
            ("id".to_string(), id.clone()),
            ("result".to_string(), result),
        ]);
        self.send_frame(frame)
    }

    /// Answer a server-to-client request with an error frame.
    pub(crate) fn respond_error(
        &self,
        id: &Value,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Result<(), SdkError> {
        if let Some(err) = self.closed_error() {
            return Err(err);
        }
        let mut error = Map::from_iter([
            ("code".to_string(), Value::from(code)),
            ("message".to_string(), Value::from(message)),
        ]);
        if let Some(data) = data {
            error.insert("data".to_string(), data);
        }
        let frame = Map::from_iter([
            ("jsonrpc".to_string(), Value::from("2.0")),
            ("id".to_string(), id.clone()),
            ("error".to_string(), Value::Object(error)),
        ]);
        self.send_frame(frame)
    }

    /// Write one response frame to the runtime's stdin.
    fn send_frame(&self, frame: Map<String, Value>) -> Result<(), SdkError> {
        let frame = serde_json::to_string(&frame).expect("json value serialization cannot fail");
        if self.out_tx.send(OutFrame::Line(frame)).is_err() {
            return Err(self.closed_error().unwrap_or_else(|| {
                SdkError::transport_closed("DeepSeek Harness runtime stdin closed", None, &[])
            }));
        }
        Ok(())
    }

    /// Send a request frame and await its response, honoring the configured
    /// request timeout when set.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, SdkError> {
        self.request_with_timeout(method, params, self.request_timeout)
            .await
    }

    /// Send a request frame and await its response with an explicit timeout;
    /// `None` waits indefinitely.
    pub(crate) async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, SdkError> {
        if let Some(err) = self.closed_error() {
            return Err(err);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), sender);

        let mut frame = Map::from_iter([
            ("jsonrpc".to_string(), Value::from("2.0")),
            ("id".to_string(), Value::from(id.clone())),
            ("method".to_string(), Value::from(method)),
        ]);
        if let Some(params) = params {
            frame.insert("params".to_string(), params);
        }
        let frame = serde_json::to_string(&frame).expect("json value serialization cannot fail");
        if self.out_tx.send(OutFrame::Line(frame)).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(self.closed_error().unwrap_or_else(|| {
                SdkError::transport_closed("DeepSeek Harness runtime stdin closed", None, &[])
            }));
        }

        let wait = async {
            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(self.closed_error().unwrap_or_else(|| {
                    SdkError::transport_closed("DeepSeek Harness runtime closed", None, &[])
                })),
            }
        };
        match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, wait).await {
                Ok(result) => result,
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    Err(SdkError::RequestTimeout {
                        message: format!("{method} timed out waiting for DeepSeek Harness runtime"),
                    })
                }
            },
            None => wait.await,
        }
    }

    /// Send a notification frame to the runtime.
    pub(crate) fn notify(&self, method: &str, params: Option<Value>) -> Result<(), SdkError> {
        if let Some(err) = self.closed_error() {
            return Err(err);
        }
        let mut frame = Map::from_iter([
            ("jsonrpc".to_string(), Value::from("2.0")),
            ("method".to_string(), Value::from(method)),
        ]);
        if let Some(params) = params {
            frame.insert("params".to_string(), params);
        }
        let frame = serde_json::to_string(&frame).expect("json value serialization cannot fail");
        if self.out_tx.send(OutFrame::Line(frame)).is_err() {
            return Err(self.closed_error().unwrap_or_else(|| {
                SdkError::transport_closed("DeepSeek Harness runtime stdin closed", None, &[])
            }));
        }
        Ok(())
    }

    /// Register a subscription; each matching notification is delivered once.
    pub(crate) fn subscribe(&self, filter: Option<NotificationFilter>) -> NotificationSubscription {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let (tx, rx) = mpsc::unbounded_channel();
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.shared
            .subscribers
            .lock()
            .unwrap()
            .insert(id, SubscriberSlot { tx, filter });
        NotificationSubscription {
            rx,
            shared: self.shared.clone(),
            id,
            closed: None,
        }
    }

    /// Subscribe to the unmatched-notification queue (slot `0`): notifications
    /// no other subscriber matched. The returned subscription is permanent; it
    /// does not unregister on drop.
    pub(crate) fn subscribe_default(&self) -> NotificationSubscription {
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut subscribers = self.shared.subscribers.lock().unwrap();
            let slot = subscribers
                .get_mut(&0)
                .expect("the unmatched queue exists for the peer's lifetime");
            slot.tx = tx;
        }
        NotificationSubscription {
            rx,
            shared: self.shared.clone(),
            id: 0,
            closed: None,
        }
    }

    /// Request stdin EOF; the writer task drops its write half and exits.
    pub(crate) fn close_stdin(&self) {
        let _ = self.out_tx.send(OutFrame::CloseStdin);
    }

    /// Wait until the reader observes EOF or a write fails.
    pub(crate) async fn wait_closed(&self) {
        self.shared.eof.notified().await;
    }

    /// Record the terminal error once, fail every pending request with it,
    /// and push it to every subscriber and the incoming-request queue.
    /// Idempotent: only the first error wins.
    pub(crate) fn fail_closed(&self, error: SdkError) {
        {
            let mut slot = self.shared.closed_err.lock().unwrap();
            if slot.is_none() {
                *slot = Some(error.clone());
            }
        }
        self.shared.eof.notify_waiters();
        let pending: Vec<_> = {
            // std Mutex: no await inside the critical section.
            let map = self.pending.try_lock();
            match map {
                Ok(mut map) => map.drain().map(|(_, sender)| sender).collect(),
                Err(_) => Vec::new(),
            }
        };
        for sender in pending {
            let _ = sender.send(Err(error.clone()));
        }
        let subscribers: Vec<_> = {
            let mut subscribers = self.shared.subscribers.lock().unwrap();
            subscribers.drain().map(|(_, slot)| slot.tx).collect()
        };
        for tx in subscribers {
            let _ = tx.send(SubMsg::Closed(error.clone()));
        }
        let _ = self
            .shared
            .requests
            .send(IncomingMsg::Closed(error.clone()));
    }

    fn closed_error(&self) -> Option<SdkError> {
        self.shared.closed_err.lock().unwrap().clone()
    }
}

/// A notification subscription; `next()` waits for the next matching
/// notification and `try_next()` polls without blocking. When the runtime
/// closes, the subscription delivers the terminal error after draining
/// queued notifications, mirroring the Python SDK's per-subscriber queues.
pub struct NotificationSubscription {
    rx: mpsc::UnboundedReceiver<SubMsg>,
    shared: Arc<PeerShared>,
    id: u64,
    closed: Option<SdkError>,
}

impl NotificationSubscription {
    /// Wait for the next matching notification, or the terminal error.
    pub async fn next(&mut self) -> Result<Notification, SdkError> {
        if let Some(error) = self.closed.clone() {
            return Err(error);
        }
        match self.rx.recv().await {
            Some(SubMsg::Notification(notification)) => Ok(notification),
            Some(SubMsg::Closed(error)) => {
                self.closed = Some(error.clone());
                Err(error)
            }
            None => {
                let error = self.closed_error();
                self.closed = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Poll for the next matching notification; `None` when the queue is
    /// empty. A terminal error surfaces on the next `next()` call.
    pub fn try_next(&mut self) -> Option<Notification> {
        match self.rx.try_recv() {
            Ok(SubMsg::Notification(notification)) => Some(notification),
            Ok(SubMsg::Closed(error)) => {
                self.closed = Some(error);
                None
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                let error = self.closed_error();
                self.closed = Some(error.clone());
                None
            }
        }
    }

    fn closed_error(&self) -> SdkError {
        self.shared
            .closed_err
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                SdkError::transport_closed("DeepSeek Harness runtime closed", None, &[])
            })
    }
}

impl Drop for NotificationSubscription {
    fn drop(&mut self) {
        // Slot 0 is the permanent unmatched-notification queue.
        if self.id != 0 {
            self.shared.subscribers.lock().unwrap().remove(&self.id);
        }
    }
}

/// The pending-map key for a frame id: strings verbatim, numbers as their
/// decimal spelling, matching the Python SDK's string-keyed correlation.
fn id_key(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

async fn stdin_write<W: AsyncWrite + Unpin>(stdin: &mut W, frame: &[u8]) -> std::io::Result<()> {
    stdin.write_all(frame).await?;
    stdin.flush().await
}
