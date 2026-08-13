//! Runtime acquisition: the feature-gated downloader that fetches the
//! platform runtime wheel from a PyPI-style index, verifies its SHA-256
//! digest, and extracts the executable (plus the macOS spawn helper and the
//! default `cordis.yml`) into a versioned cache.
//!
//! The build workflow retains platform wheels and nothing else, so PyPI is
//! the only published artifact stream carrying the runtime executable today.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::SdkError;

/// Inputs for [`resolve`].
#[derive(Debug, Default)]
pub struct ResolveOptions {
    /// PyPI-style index; default `https://pypi.org/pypi` (or `$DSH_RUNTIME_PYPI_URL`).
    pub index_url: Option<String>,
    /// Runtime wheel version; default the crate version.
    pub version: Option<String>,
    /// Cache directory; default the platform cache directory (or `$DSH_RUNTIME_CACHE_DIR`).
    pub cache_dir: Option<PathBuf>,
}

/// The resolved runtime executable plus its default configuration.
#[derive(Debug)]
pub struct ResolvedRuntime {
    /// argv tuple launching the runtime.
    pub launch_args: Vec<String>,
    /// The extracted default `cordis.yml`.
    pub default_config: PathBuf,
}

/// Wheel tag and executable suffix for the current target.
fn platform() -> Result<(&'static str, &'static str), SdkError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(("py3-none-macosx_14_0_arm64", "macos-arm64")),
        ("linux", "x86_64") => Ok(("py3-none-manylinux_2_28_x86_64", "linux-x64")),
        ("linux", "aarch64") => Ok(("py3-none-manylinux_2_28_aarch64", "linux-arm64")),
        (os, arch) => Err(SdkError::RuntimeResolve {
            message: format!(
                "no deepseek-harness-runtime-bin wheel exists for this platform \
                 (os={os}, arch={arch}); supported: macos/arm64, linux/x64, linux/arm64. \
                 Provide HarnessClientOptions.runtime_bin instead."
            ),
        }),
    }
}

/// Resolve the platform runtime: reuse a verified cache entry or fetch,
/// verify, and extract the platform wheel.
pub fn resolve(options: &ResolveOptions) -> Result<ResolvedRuntime, SdkError> {
    let (wheel_tag, exe_tag) = platform()?;
    let version = options
        .version
        .clone()
        .or_else(|| std::env::var("DSH_RUNTIME_VERSION").ok())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let index = options
        .index_url
        .clone()
        .or_else(|| std::env::var("DSH_RUNTIME_PYPI_URL").ok())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| "https://pypi.org/pypi".to_string());
    let index = index.trim_end_matches('/');

    let cache_root = options
        .cache_dir
        .clone()
        .or_else(|| std::env::var_os("DSH_RUNTIME_CACHE_DIR").map(PathBuf::from))
        .or_else(|| dirs::cache_dir().map(|dir| dir.join("deepseek-harness-sdk-rs")))
        .ok_or_else(|| SdkError::RuntimeResolve {
            message: "no cache directory for the runtime download; set $DSH_RUNTIME_CACHE_DIR"
                .into(),
        })?;
    let cache_dir = cache_root.join(&version);
    let exe_name = format!("dsh-jsonrpc-agent-pkg-{exe_tag}");
    let exe_path = cache_dir.join(&exe_name);
    let config_path = cache_dir.join("cordis.yml");

    if cache_valid(&cache_dir, &exe_path, &exe_name) {
        return Ok(ResolvedRuntime {
            launch_args: vec![exe_path.to_string_lossy().into_owned()],
            default_config: config_path,
        });
    }

    let file = fetch_wheel(index, &version, wheel_tag)?;
    let bytes = download(&file.url)?;
    verify_digest(&bytes, &file.sha256)?;
    extract(&cache_dir, &exe_name, &bytes)?;
    Ok(ResolvedRuntime {
        launch_args: vec![exe_path.to_string_lossy().into_owned()],
        default_config: config_path,
    })
}

struct WheelFile {
    url: String,
    sha256: String,
}

/// Fetch the index metadata and pick the wheel matching this platform's tag.
fn fetch_wheel(index: &str, version: &str, wheel_tag: &str) -> Result<WheelFile, SdkError> {
    let url = format!("{index}/deepseek-harness-runtime-bin/{version}/json");
    let mut response = ureq::get(&url)
        .call()
        .map_err(|error| SdkError::RuntimeResolve {
            message: format!("failed to fetch runtime index {url}: {error}"),
        })?;
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|error| SdkError::RuntimeResolve {
            message: format!("failed to read runtime index {url}: {error}"),
        })?;
    let metadata: Value =
        serde_json::from_slice(&bytes).map_err(|error| SdkError::RuntimeResolve {
            message: format!("runtime index {url} is not valid JSON: {error}"),
        })?;
    let urls = metadata
        .get("urls")
        .and_then(Value::as_array)
        .ok_or_else(|| SdkError::RuntimeResolve {
            message: format!("runtime index {url} has no urls array"),
        })?;
    let suffix = format!("{wheel_tag}.whl");
    for entry in urls {
        let Some(filename) = entry.get("filename").and_then(Value::as_str) else {
            continue;
        };
        if !filename.ends_with(&suffix) {
            continue;
        }
        let url =
            entry
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| SdkError::RuntimeResolve {
                    message: format!("wheel entry {filename} has no url"),
                })?;
        let sha256 = entry
            .get("digests")
            .and_then(|digests| digests.get("sha256"))
            .and_then(Value::as_str)
            .ok_or_else(|| SdkError::RuntimeResolve {
                message: format!("wheel entry {filename} has no sha256 digest"),
            })?;
        return Ok(WheelFile {
            url: url.to_string(),
            sha256: sha256.to_string(),
        });
    }
    Err(SdkError::RuntimeResolve {
        message: format!(
            "deepseek-harness-runtime-bin {version} has no wheel for platform tag {wheel_tag}; \
             provide HarnessClientOptions.runtime_bin instead"
        ),
    })
}

/// Ceiling on the in-memory wheel download; the current wheel is ~174 MB, so
/// the bound leaves headroom while keeping ureq's 10 MB default off the path.
const RUNTIME_WHEEL_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Download the wheel into memory; the exe is ~174 MB, so this is the only
/// large allocation in the module and is never cached in that form.
fn download(url: &str) -> Result<Vec<u8>, SdkError> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| SdkError::RuntimeResolve {
            message: format!("failed to download runtime wheel {url}: {error}"),
        })?;
    response
        .body_mut()
        .with_config()
        .limit(RUNTIME_WHEEL_MAX_BYTES)
        .read_to_vec()
        .map_err(|error| SdkError::RuntimeResolve {
            message: format!("failed to read runtime wheel {url}: {error}"),
        })
}

/// Verify the wheel bytes against the published SHA-256 digest.
fn verify_digest(bytes: &[u8], expected_hex: &str) -> Result<(), SdkError> {
    let expected = hex::decode(expected_hex).map_err(|error| SdkError::RuntimeResolve {
        message: format!("invalid sha256 digest {expected_hex:?}: {error}"),
    })?;
    use sha2::Digest;
    let actual = sha2::Sha256::digest(bytes);
    if actual.as_slice() != expected.as_slice() {
        return Err(SdkError::RuntimeResolve {
            message: "downloaded runtime wheel failed its sha256 verification".into(),
        });
    }
    Ok(())
}

/// Extract the executable, the macOS spawn helper where applicable, and the
/// default `cordis.yml` into the cache directory; the `.complete` marker is
/// written last so an interrupted extraction never reads as a valid cache.
fn extract(cache_dir: &Path, exe_name: &str, bytes: &[u8]) -> Result<(), SdkError> {
    let staging = cache_dir.with_file_name(format!(
        "{}.tmp-{}",
        cache_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("stage"),
        std::process::id()
    ));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let cleanup = |staging: &Path| {
        let _ = std::fs::remove_dir_all(staging);
    };

    let result = (|| -> Result<(), SdkError> {
        std::fs::create_dir_all(&staging)?;
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| SdkError::RuntimeResolve {
                message: format!("runtime wheel is not a readable zip: {error}"),
            })?;
        let exe_entry = format!("deepseek_harness_runtime/runtime/{exe_name}");
        write_entry(&mut archive, &exe_entry, &staging.join(exe_name))?;
        if exe_name.starts_with("macos-") {
            let helper_entry = format!("deepseek_harness_runtime/runtime/{exe_name}-spawn-helper");
            write_entry(
                &mut archive,
                &helper_entry,
                &staging.join(format!("{exe_name}-spawn-helper")),
            )?;
        }
        write_entry(
            &mut archive,
            "deepseek_harness_runtime/runtime/cordis.yml",
            &staging.join("cordis.yml"),
        )?;
        std::fs::write(staging.join(".complete"), b"")?;
        if cache_dir.exists() {
            let _ = std::fs::remove_dir_all(cache_dir);
        }
        if let Some(parent) = cache_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&staging, cache_dir)?;
        Ok(())
    })();

    if result.is_err() {
        cleanup(&staging);
    }
    result
}

fn write_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    entry: &str,
    dest: &Path,
) -> Result<(), SdkError> {
    let mut file = archive
        .by_name(entry)
        .map_err(|error| SdkError::RuntimeResolve {
            message: format!("runtime wheel is missing {entry}: {error}"),
        })?;
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(&mut file, &mut out)?;
    drop(out);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// A cache entry is valid when the marker, the executable, and (on macOS)
/// the spawn helper all exist.
fn cache_valid(cache_dir: &Path, exe_path: &Path, exe_name: &str) -> bool {
    if !cache_dir.join(".complete").is_file() || !exe_path.is_file() {
        return false;
    }
    if exe_name.starts_with("macos-") {
        let helper = exe_path.with_file_name(format!("{exe_name}-spawn-helper"));
        if !helper.is_file() {
            return false;
        }
    }
    true
}
