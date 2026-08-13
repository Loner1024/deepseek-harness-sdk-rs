//! Shared helpers for the SDK integration tests.
#![allow(dead_code)]

use deepseek_harness_sdk_rs::HarnessClientOptions;

/// Absolute path of the `dsh-fake-runtime` test binary.
///
/// Panics with guidance when the binary was not built: the mechanism-tier
/// tests require `cargo test --features test-support`.
pub fn fake_runtime() -> String {
    std::env::var("CARGO_BIN_EXE_dsh-fake-runtime").unwrap_or_else(|_| {
        panic!(
            "CARGO_BIN_EXE_dsh-fake-runtime is unset: \
             run `cargo test --features test-support` so the test binary is built"
        )
    })
}

/// Low-level client options launching the fake runtime in `scenario`.
pub fn client_options(scenario: &str) -> HarnessClientOptions {
    HarnessClientOptions {
        command: Some(fake_runtime()),
        args: vec![scenario.to_string()],
        ..Default::default()
    }
}

/// The runtime exe name (without directory) for the current platform, plus
/// whether the wheel must carry the macOS spawn helper.
pub fn platform_exe() -> (String, bool) {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("dsh-jsonrpc-agent-pkg-macos-arm64".to_string(), true),
        ("linux", "x86_64") => ("dsh-jsonrpc-agent-pkg-linux-x64".to_string(), false),
        ("linux", "aarch64") => ("dsh-jsonrpc-agent-pkg-linux-arm64".to_string(), false),
        (os, arch) => panic!("no downloader fixture wheel for os={os} arch={arch}"),
    }
}

/// The PyPI platform tag matching [`platform_exe`].
pub fn platform_tag() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "py3-none-macosx_14_0_arm64",
        ("linux", "x86_64") => "py3-none-manylinux_2_28_x86_64",
        ("linux", "aarch64") => "py3-none-manylinux_2_28_aarch64",
        (os, arch) => panic!("no downloader fixture wheel for os={os} arch={arch}"),
    }
}
