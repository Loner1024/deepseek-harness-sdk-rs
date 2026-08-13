//! Error taxonomy of the SDK client, mirroring the Python SDK's error classes.

use std::sync::Arc;

use serde_json::Value;

/// Errors surfaced by the SDK.
///
/// The variants reproduce the Python SDK's taxonomy: [`JsonRpcResponse`]
/// (`JsonRpcError`), [`RequestTimeout`] (`TimeoutError`), [`Protocol`]
/// (`SdkProtocolError`), and [`TransportClosed`] (`TransportClosedError`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SdkError {
    /// The runtime answered a request with a JSON-RPC error response;
    /// `code` and `data` are preserved from the protocol frame.
    #[error("{message}")]
    JsonRpcResponse {
        /// Error code from the response, if it was a JSON integer.
        code: Option<i64>,
        /// Error message from the response.
        message: String,
        /// Optional error data from the response.
        data: Option<Value>,
    },
    /// The configured request deadline elapsed with no response.
    #[error("{message}")]
    RequestTimeout {
        /// Human-readable timeout report.
        message: String,
    },
    /// The runtime sent data outside the documented protocol.
    #[error("{message}")]
    Protocol {
        /// Human-readable protocol violation report.
        message: String,
    },
    /// The runtime subprocess exited or closed its stdout.
    #[error("{message}")]
    TransportClosed {
        /// Human-readable report, carrying the exit code and stderr tail below.
        message: String,
        /// Process exit code when the runtime was observed exited, if any.
        exit_code: Option<i32>,
        /// Bounded tail (last 400 lines) of the runtime's stderr.
        stderr_tail: Vec<String>,
    },
    /// I/O failure talking to the runtime or the local filesystem.
    ///
    /// Wrapped in an `Arc` so the error type stays `Clone` for fan-out to
    /// every pending waiter.
    #[error("io error: {source}")]
    Io {
        #[from]
        source: Arc<std::io::Error>,
    },
    /// The blocking facade was called from inside an async runtime context.
    #[error("the deepseek-harness-sdk-rs sync API cannot be called from within an async runtime")]
    NestedRuntime,
    /// Runtime acquisition (the downloader) failed.
    #[error("{message}")]
    RuntimeResolve {
        /// Human-readable resolution failure report.
        message: String,
    },
}

impl From<std::io::Error> for SdkError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            source: Arc::new(error),
        }
    }
}

impl SdkError {
    /// Build a [`SdkError::TransportClosed`] whose message carries the reason,
    /// the exit code when known, and the bounded stderr tail when non-empty —
    /// the Python SDK's diagnostics format.
    pub(crate) fn transport_closed(
        reason: &str,
        exit_code: Option<i32>,
        stderr_tail: &[String],
    ) -> Self {
        let mut parts = vec![reason.to_string()];
        if let Some(code) = exit_code {
            parts.push(format!("exit code: {code}"));
        }
        if !stderr_tail.is_empty() {
            parts.push(format!("stderr tail:\n{}", stderr_tail.join("\n")));
        }
        Self::TransportClosed {
            message: parts.join("\n"),
            exit_code,
            stderr_tail: stderr_tail.to_vec(),
        }
    }

    /// Build a [`SdkError::Protocol`] for a response outside the documented protocol.
    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}
