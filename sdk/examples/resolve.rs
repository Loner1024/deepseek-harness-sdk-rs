//! Resolve the platform runtime through the downloader and print its paths.
//!
//! ```sh
//! cargo run --package deepseek-harness-sdk-rs --example resolve
//! ```
//!
//! Overrides: `DSH_RUNTIME_PYPI_URL`, `DSH_RUNTIME_VERSION`,
//! `DSH_RUNTIME_CACHE_DIR`. The default version is the crate version; pass
//! `DSH_RUNTIME_VERSION` when the runtime release differs.

use deepseek_harness_sdk_rs::runtime::{ResolveOptions, resolve};

fn main() {
    match resolve(&ResolveOptions {
        index_url: None,
        version: None,
        cache_dir: None,
    }) {
        Ok(resolved) => {
            println!("exe={}", resolved.launch_args[0]);
            println!("cordis={}", resolved.default_config.display());
        }
        Err(error) => {
            eprintln!("resolve failed: {error}");
            std::process::exit(1);
        }
    }
}
