//! Single-task example: run one task through the harness runtime.
//!
//! The `runtime-download` feature fetches the platform runtime wheel on first
//! use and caches it; the runtime reads the credential from the environment.
//! Every SDK launch needs an explicit Harness home; this example uses
//! `$DSH_HOME` when set and a temporary home otherwise.
//!
//! ```sh
//! export DEEPSEEK_API_KEY=sk-your-key-here
//! # export DSH_HOME=/absolute/path/to/isolated-dsh-home
//! # export DEEPSEEK_BASE_URL=http://127.0.0.1:8000/v1
//! cargo run --package deepseek-harness-sdk-rs --example simple -- "Inspect this directory."
//! ```

use std::path::PathBuf;

use deepseek_harness_sdk_rs::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions, SdkError};

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Say hello.".to_string());
    let dsh_home = std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("dsh-sdk-example-home"));
    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        dsh_home: Some(dsh_home),
        ..Default::default()
    });
    let result = harness.run(task, &RunOptions::default()).await?;
    println!("{}", result.final_response);
    harness.close().await;
    Ok(())
}
