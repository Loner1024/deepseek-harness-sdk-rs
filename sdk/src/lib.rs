//! # deepseek-harness-sdk
//!
//! Drive a DeepSeek Harness runtime from Rust. The SDK spawns the runtime as
//! a subprocess and speaks the line-delimited JSON-RPC 2.0 protocol over its
//! stdio ([protocol reference](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/sdk/protocol/README.md));
//! the runtime is the single-file executable also distributed with the
//! [Python SDK](../../../python/README.md).
//!
//! ```no_run
//! use deepseek_harness_sdk::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};
//!
//! # async fn example() -> Result<(), deepseek_harness_sdk::SdkError> {
//! let harness = DeepSeekHarness::new(DeepSeekHarnessConfig::default());
//! let result = harness.run("say hi", &RunOptions::default()).await?;
//! println!("{}", result.final_response);
//! harness.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! Without any launch configuration the `runtime-download` feature (on by
//! default) fetches the platform runtime wheel from PyPI, verifies its
//! SHA-256 digest, and caches the executable. Callers who spawn their own
//! runtime pass `runtime_bin` or `launch_args_override` and can disable the
//! feature. A blocking facade ([`DeepSeekHarnessSync`]) covers synchronous
//! callers.

pub mod api;
pub mod client;
pub mod error;
pub mod events;
#[cfg(feature = "runtime-download")]
pub mod runtime;
mod transport;

pub use api::{
    DeepSeekHarness, DeepSeekHarnessConfig, DeepSeekHarnessSync, Input, RunOptions, RunResult,
    Session,
};
pub use client::{
    HarnessClient, HarnessClientOptions, InitializeParams, InitializeResult, Notification,
    ServerInfo,
};
pub use error::SdkError;
pub use transport::{NotificationFilter, NotificationSubscription};
