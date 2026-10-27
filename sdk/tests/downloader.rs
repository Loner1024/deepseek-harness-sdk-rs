//! Downloader tests: a local wheel server stands in for the PyPI index. The
//! served "runtime executable" is the `dsh-fake-runtime` binary itself, so the
//! resolved runtime also drives real turns end-to-end.

mod common;

use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use deepseek_harness_sdk_rs::runtime::{ResolveOptions, resolve};
use deepseek_harness_sdk_rs::{
    DeepSeekHarness, DeepSeekHarnessConfig, InitializeParams, RunOptions, SdkError,
};

fn build_wheel(
    exe_name: &str,
    sidecar: &str,
    platform_dir: &str,
    exe_bytes: &[u8],
    macos: bool,
) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(
            format!("deepseek_harness_runtime/runtime/{exe_name}"),
            options,
        )
        .expect("start exe");
        zip.write_all(exe_bytes).expect("write exe");
        zip.start_file(
            format!("deepseek_harness_runtime/runtime/{sidecar}"),
            options,
        )
        .expect("start sidecar");
        zip.write_all(b"ripgrep").expect("write sidecar");
        if macos {
            zip.start_file(
                format!("deepseek_harness_runtime/runtime/{exe_name}-spawn-helper"),
                options,
            )
            .expect("start helper");
            zip.write_all(b"helper").expect("write helper");
        }
        zip.start_file(
            format!("deepseek_harness_runtime/runtime/{platform_dir}/downloads.json"),
            options,
        )
        .expect("start downloads");
        zip.write_all(b"{}").expect("write downloads");
        zip.start_file(
            format!(
                "deepseek_harness_runtime/runtime/{platform_dir}/office-skills/office/SKILL.md"
            ),
            options,
        )
        .expect("start skill");
        zip.write_all(b"# Office\n").expect("write skill");
        // The repo-only Node carrier is excluded from the wheel payload; a
        // fixture copy proves extraction skips it.
        zip.start_file(
            "deepseek_harness_runtime/runtime/node/runtime-bootstrap.mjs",
            options,
        )
        .expect("start node carrier");
        zip.write_all(b"// dev carrier\n")
            .expect("write node carrier");
        // A legacy default composition must not be injected anywhere.
        zip.start_file("deepseek_harness_runtime/runtime/cordis.yml", options)
            .expect("start legacy config");
        zip.write_all(b"plugins: []\n")
            .expect("write legacy config");
        zip.finish().expect("finish");
    }
    buffer.into_inner()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A one-shot-ish HTTP server: `/pypi/.../json` answers an index built with
/// the request's own Host header, any other path serves the wheel bytes.
fn start_server(
    filename: String,
    wheel: Vec<u8>,
    sha256: String,
) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = hits.clone();
    let thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = [0u8; 8192];
            let n = stream.read(&mut buffer).unwrap_or(0);
            if n == 0 {
                continue;
            }
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let body: Vec<u8> = if path.ends_with("/json") {
                let host = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("host").then(|| value.trim())
                    })
                    .unwrap_or("127.0.0.1");
                serde_json::json!({
                    "info": {"version": "9.9.9"},
                    "urls": [{
                        "filename": filename,
                        "url": format!("http://{host}/files/{filename}"),
                        "digests": {"sha256": sha256}
                    }]
                })
                .to_string()
                .into_bytes()
            } else {
                wheel.clone()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
    });
    (format!("127.0.0.1:{port}"), hits, thread)
}

fn wheel_filename() -> String {
    format!(
        "deepseek_harness_runtime_bin-9.9.9-{}.whl",
        common::platform_tag()
    )
}

fn fixture_wheel() -> (Vec<u8>, String, String, bool) {
    let exe_bytes = std::fs::read(common::fake_runtime()).expect("fake runtime binary");
    let (exe_name, sidecar, macos) = common::platform_exe();
    let wheel = build_wheel(
        &exe_name,
        &sidecar,
        common::platform_dir(),
        &exe_bytes,
        macos,
    );
    (wheel, exe_name, sidecar, macos)
}

#[tokio::test]
async fn resolver_extracts_verified_runtime_and_drives_a_turn() {
    let (wheel, exe_name, sidecar, _macos) = fixture_wheel();
    let (base, _hits, thread) = start_server(wheel_filename(), wheel.clone(), sha256_hex(&wheel));
    let cache = tempfile::TempDir::new().expect("tempdir");

    let resolved = resolve(&ResolveOptions {
        index_url: Some(format!("http://{base}/pypi")),
        version: Some("9.9.9".to_string()),
        cache_dir: Some(cache.path().to_path_buf()),
    })
    .expect("resolve");

    assert_eq!(resolved.launch_args.len(), 1);
    let exe_path = PathBuf::from(&resolved.launch_args[0]);
    assert!(exe_path.ends_with(&exe_name));
    assert!(exe_path.is_file());
    let cache_dir = cache.path().join("9.9.9");
    assert!(cache_dir.join(&sidecar).is_file(), "ripgrep sidecar");
    assert!(
        cache_dir
            .join(common::platform_dir())
            .join("downloads.json")
            .is_file(),
        "platform resource directory"
    );
    assert!(
        !cache_dir.join("node").exists(),
        "the repo-only Node carrier must not be extracted"
    );
    assert!(cache_dir.join(".complete").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&exe_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "extracted runtime must be executable");
        let sidecar_mode = std::fs::metadata(cache_dir.join(&sidecar))
            .expect("sidecar metadata")
            .permissions()
            .mode();
        assert_ne!(
            sidecar_mode & 0o111,
            0,
            "extracted sidecar must be executable"
        );
    }

    // The downloaded "runtime" is the fake runtime: a full turn runs through
    // the managed launch, which appends `--profile sdk`.
    let home = tempfile::TempDir::new().expect("home");
    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        runtime_bin: Some(exe_path),
        dsh_home: Some(home.path().to_path_buf()),
        ..Default::default()
    });
    let result = harness
        .run("hi", &RunOptions::default())
        .await
        .expect("run via resolved runtime");
    assert_eq!(result.final_response, "hello from fake");
    harness.close().await;

    drop(thread);
}

#[tokio::test]
async fn resolver_reuses_a_verified_cache() {
    let (wheel, _exe_name, _sidecar, _macos) = fixture_wheel();
    let (base, hits, thread) = start_server(wheel_filename(), wheel.clone(), sha256_hex(&wheel));
    let cache = tempfile::TempDir::new().expect("tempdir");
    let options = ResolveOptions {
        index_url: Some(format!("http://{base}/pypi")),
        version: Some("9.9.9".to_string()),
        cache_dir: Some(cache.path().to_path_buf()),
    };

    let first = resolve(&options).expect("first resolve");
    let second = resolve(&options).expect("second resolve");
    assert_eq!(first.launch_args, second.launch_args);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "one index request plus one wheel download"
    );

    drop(thread);
}

#[tokio::test]
async fn resolver_rejects_a_digest_mismatch() {
    let (wheel, _exe_name, _sidecar, _macos) = fixture_wheel();
    let bad_digest = "ab".repeat(32);
    let (base, _hits, thread) = start_server(wheel_filename(), wheel, bad_digest);
    let cache = tempfile::TempDir::new().expect("tempdir");

    let error = resolve(&ResolveOptions {
        index_url: Some(format!("http://{base}/pypi")),
        version: Some("9.9.9".to_string()),
        cache_dir: Some(cache.path().to_path_buf()),
    })
    .unwrap_err();
    assert!(matches!(error, SdkError::RuntimeResolve { .. }));
    assert!(
        !cache.path().join("9.9.9").exists(),
        "failed verification must not leave a cache entry"
    );

    drop(thread);
}

#[tokio::test]
async fn resolver_fails_loud_without_a_matching_wheel() {
    let (wheel, _exe_name, _sidecar, _macos) = fixture_wheel();
    let foreign = "deepseek_harness_runtime_bin-9.9.9-py3-none-fake_platform.whl".to_string();
    let (base, _hits, thread) = start_server(foreign, wheel.clone(), sha256_hex(&wheel));
    let cache = tempfile::TempDir::new().expect("tempdir");

    let error = resolve(&ResolveOptions {
        index_url: Some(format!("http://{base}/pypi")),
        version: Some("9.9.9".to_string()),
        cache_dir: Some(cache.path().to_path_buf()),
    })
    .unwrap_err();
    assert!(matches!(error, SdkError::RuntimeResolve { .. }));

    drop(thread);
}

#[tokio::test]
async fn downloaded_runtime_receives_profile_and_home() {
    let (wheel, _exe_name, _sidecar, _macos) = fixture_wheel();
    let (base, _hits, thread) = start_server(wheel_filename(), wheel.clone(), sha256_hex(&wheel));
    let cache = tempfile::TempDir::new().expect("tempdir");
    let home = tempfile::TempDir::new().expect("home");

    let harness = DeepSeekHarness::new(DeepSeekHarnessConfig {
        runtime_index_url: Some(format!("http://{base}/pypi")),
        runtime_version: Some("9.9.9".to_string()),
        runtime_cache_dir: Some(cache.path().to_path_buf()),
        dsh_home: Some(home.path().to_path_buf()),
        env: [
            ("DSH_FAKE_SCENARIO".to_string(), "launch".to_string()),
            ("DSH_CORDIS_CONFIG".to_string(), String::new()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    harness.start().await.expect("start");
    let report = harness
        .client()
        .initialize(&InitializeParams {
            cwd: std::env::current_dir().expect("cwd"),
            provider: "p".to_string(),
            model: "m".to_string(),
            reasoning_effort: None,
            max_tokens: None,
            approval_requests: false,
        })
        .await
        .expect("launch report");
    let report: serde_json::Value = serde_json::from_str(
        report
            .server_info
            .expect("serverInfo")
            .version
            .expect("version")
            .as_str(),
    )
    .expect("launch report JSON");
    assert_eq!(
        report.get("args"),
        Some(&serde_json::json!(["--profile", "sdk"]))
    );
    let expected_home = home.path().canonicalize().expect("canonical home");
    assert_eq!(
        report.get("dsh_home").and_then(|value| value.as_str()),
        expected_home.to_str()
    );
    assert_eq!(
        report.get("cordis"),
        Some(&serde_json::Value::Null),
        "the legacy cordis.yml must not be injected"
    );
    harness.close().await;

    drop(thread);
}
