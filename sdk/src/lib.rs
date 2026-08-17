//! # deepseek-harness-sdk-rs
//!
//! Drive a DeepSeek Harness runtime from Rust. The SDK spawns the runtime as
//! a subprocess and speaks the line-delimited JSON-RPC 2.0 protocol over its
//! stdio ([protocol reference](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/sdk/protocol/README.md));
//! the runtime is the single-file executable also distributed with the
//! [Python SDK](../../../python/README.md).
//!
//! ```no_run
//! use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions};
//!
//! # async fn example() -> Result<(), deepseek_harness_sdk_rs::SdkError> {
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

use std::path::{Component, Path, PathBuf};

/// Resolve a path the way Python's `Path.resolve(strict=False)` does: make it
/// absolute, collapse `.` and `..` lexically, resolve symlinks along the
/// existing prefix, and keep any not-yet-existing tail verbatim. The SDK
/// applies this to `cwd` and `runtime_cwd` before subprocess launch,
/// environment injection, and the wire handshake.
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let normalized = collapse_dot_components(absolute);
    if normalized.exists() {
        return normalized.canonicalize().unwrap_or(normalized);
    }
    let mut missing = Vec::new();
    let mut prefix = normalized.as_path();
    while !prefix.exists() {
        let Some(parent) = prefix.parent() else {
            break;
        };
        let Some(name) = prefix.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        prefix = parent;
    }
    let mut resolved = prefix
        .canonicalize()
        .unwrap_or_else(|_| prefix.to_path_buf());
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    resolved
}

/// Lexically collapse `.` and `..` components, clamping `..` at the root the
/// way `Path.resolve()` does. No filesystem access.
fn collapse_dot_components(path: PathBuf) -> PathBuf {
    let mut collapsed = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    collapsed.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    collapsed.pop();
                }
                // At the root, a parent component stays at the root.
            }
            other => collapsed.push(other.as_os_str()),
        }
    }
    collapsed
}

pub use api::{
    DeepSeekHarness, DeepSeekHarnessConfig, DeepSeekHarnessSync, Input, RunOptions, RunResult,
    Session,
};
pub use client::{
    HarnessClient, HarnessClientOptions, IncomingRequest, InitializeParams, InitializeResult,
    Notification, ServerInfo,
};
pub use error::SdkError;
pub use transport::{NotificationFilter, NotificationSubscription};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_collapses_parent_components_like_python() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let expected = temp
            .path()
            .canonicalize()
            .expect("canonical tempdir")
            .join("bar");
        assert_eq!(
            resolve_path(&temp.path().join("missing").join("..").join("bar")),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_path_clamps_parent_components_at_the_root() {
        assert_eq!(resolve_path(Path::new("/")), PathBuf::from("/"));
        assert_eq!(
            resolve_path(&Path::new("/..").join("..").join("etc")),
            Path::new("/etc").canonicalize().expect("canonical /etc")
        );
    }
}
