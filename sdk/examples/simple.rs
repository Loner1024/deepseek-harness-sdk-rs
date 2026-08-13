//! Zero-config example: run one task through the harness runtime.
//!
//! The `runtime-download` feature fetches the platform runtime wheel on first
//! use and caches it; the runtime reads the credential from the environment.
//!
//! ```sh
//! export DEEPSEEK_API_KEY=sk-your-key-here
//! # export DEEPSEEK_BASE_URL=http://127.0.0.1:8000/v1
//! cargo run --package deepseek-harness-sdk --example simple -- "Inspect this directory."
//! ```

use deepseek_harness_sdk::{DeepSeekHarness, DeepSeekHarnessConfig, RunOptions, SdkError};

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Say hello.".to_string());
    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig::default());
    let result = harness.run(task, &RunOptions::default()).await?;
    println!("{}", result.final_response);
    harness.close().await;
    Ok(())
}
