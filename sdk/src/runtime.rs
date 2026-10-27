//! Runtime acquisition: the feature-gated downloader that fetches the
//! platform runtime wheel from a PyPI-style index, verifies its SHA-256
//! digest, and extracts the packaged `dsh` executable, its sidecars, and the
//! platform resource directory into a versioned cache.
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

/// The resolved runtime executable.
#[derive(Debug)]
pub struct ResolvedRuntime {
    /// argv tuple launching the executable; the client appends `--profile`
    /// and patch arguments.
    pub launch_args: Vec<String>,
}

/// Wheel tag and packaged executable name for the current target.
struct Platform {
    wheel_tag: &'static str,
    exe_name: &'static str,
}

/// Normalize a semver pre-release to PEP 440 spelling for PyPI queries:
/// cargo spells `0.2.1-rc.2` while the wheel index spells `0.2.1rc2`.
fn to_pypi_version(version: &str) -> String {
    for (from, to) in [("-rc.", "rc"), ("-beta.", "b"), ("-alpha.", "a")] {
        if let Some(pos) = version.find(from) {
            let mut out = version.to_string();
            out.replace_range(pos..pos + from.len(), to);
            return out;
        }
    }
    version.to_string()
}

fn platform() -> Result<Platform, SdkError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(Platform {
            wheel_tag: "py3-none-macosx_14_0_arm64",
            exe_name: "deepseek-harness-sdk-runtime-macos-arm64",
        }),
        ("macos", "x86_64") => Ok(Platform {
            wheel_tag: "py3-none-macosx_14_0_x86_64",
            exe_name: "deepseek-harness-sdk-runtime-macos-x64",
        }),
        ("linux", "x86_64") => Ok(Platform {
            wheel_tag: "py3-none-manylinux_2_28_x86_64",
            exe_name: "deepseek-harness-sdk-runtime-linux-x64",
        }),
        ("linux", "aarch64") => Ok(Platform {
            wheel_tag: "py3-none-manylinux_2_28_aarch64",
            exe_name: "deepseek-harness-sdk-runtime-linux-arm64",
        }),
        ("windows", "x86_64") => Ok(Platform {
            wheel_tag: "py3-none-win_amd64",
            exe_name: "deepseek-harness-sdk-runtime-win-x64.exe",
        }),
        (os, arch) => Err(SdkError::RuntimeResolve {
            message: format!(
                "no deepseek-harness-runtime-bin wheel exists for this platform \
                 (os={os}, arch={arch}); supported: macos/arm64, macos/x64, linux/x64, \
                 linux/arm64, windows/x64. Provide an explicit runtime executable instead."
            ),
        }),
    }
}

/// The ripgrep sidecar shipped next to the executable on every platform.
fn sidecar_name(exe_name: &str) -> String {
    match exe_name.strip_suffix(".exe") {
        Some(stem) => format!("{stem}-rg.exe"),
        None => format!("{exe_name}-rg"),
    }
}

/// macOS ships a native PTY spawn helper beside the executable.
fn spawn_helper_name(exe_name: &str) -> String {
    format!("{exe_name}-spawn-helper")
}

fn is_macos(exe_name: &str) -> bool {
    exe_name.starts_with("deepseek-harness-sdk-runtime-macos-")
}

/// Resolve the platform runtime: reuse a verified cache entry or fetch,
/// verify, and extract the platform wheel.
pub fn resolve(options: &ResolveOptions) -> Result<ResolvedRuntime, SdkError> {
    let platform = platform()?;
    let version = to_pypi_version(
        &options
            .version
            .clone()
            .or_else(|| std::env::var("DSH_RUNTIME_VERSION").ok())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
    );
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
    let exe_path = cache_dir.join(platform.exe_name);

    if cache_valid(&cache_dir, &platform) {
        return Ok(ResolvedRuntime {
            launch_args: vec![exe_path.to_string_lossy().into_owned()],
        });
    }

    let file = fetch_wheel(index, &version, platform.wheel_tag)?;
    let bytes = download(&file.url)?;
    verify_digest(&bytes, &file.sha256)?;
    extract(&cache_dir, &platform, &bytes)?;
    Ok(ResolvedRuntime {
        launch_args: vec![exe_path.to_string_lossy().into_owned()],
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
             provide an explicit runtime executable instead"
        ),
    })
}

/// Ceiling on the in-memory wheel download; the current wheel is ~90 MB, so
/// the bound leaves headroom while keeping ureq's 10 MB default off the path.
const RUNTIME_WHEEL_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Download the wheel into memory; the embedded executable is ~300 MB, so
/// this is the only large allocation in the module and is never cached in
/// that form.
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

/// Wheel-relative directory holding the executable and its runtime payload.
const RUNTIME_PREFIX: &str = "deepseek_harness_runtime/runtime/";

/// Extract the executable, sidecars, and platform resources into the cache
/// directory; the `.complete` marker is written last so an interrupted
/// extraction never reads as a valid cache.
fn extract(cache_dir: &Path, platform: &Platform, bytes: &[u8]) -> Result<(), SdkError> {
    let staging = cache_dir.with_file_name(format!(
        "{}.tmp-{}-{}",
        cache_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("stage"),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
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
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| SdkError::RuntimeResolve {
                    message: format!("runtime wheel entry {index} is unreadable: {error}"),
                })?;
            let Some(path) = entry.enclosed_name() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(RUNTIME_PREFIX) else {
                continue;
            };
            if relative.as_os_str().is_empty() || relative.starts_with("node") {
                // Skip the directory itself and the repo-only Node carrier.
                continue;
            }
            let dest = staging.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mode = entry.unix_mode();
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            drop(out);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = mode.map(|mode| mode & 0o777).unwrap_or(0o644);
                let mode = if mode & 0o111 != 0 { mode } else { 0o644 };
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
        }

        let exe_path = staging.join(platform.exe_name);
        if !exe_path.is_file() {
            return Err(SdkError::RuntimeResolve {
                message: format!(
                    "runtime wheel is missing deepseek_harness_runtime/runtime/{}",
                    platform.exe_name
                ),
            });
        }
        let sidecar = staging.join(sidecar_name(platform.exe_name));
        if !sidecar.is_file() {
            return Err(SdkError::RuntimeResolve {
                message: format!(
                    "runtime wheel is missing the ripgrep sidecar {}",
                    sidecar_name(platform.exe_name)
                ),
            });
        }
        if is_macos(platform.exe_name)
            && !staging.join(spawn_helper_name(platform.exe_name)).is_file()
        {
            return Err(SdkError::RuntimeResolve {
                message: format!(
                    "runtime wheel is missing the macOS spawn helper {}",
                    spawn_helper_name(platform.exe_name)
                ),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                &exe_path,
                &sidecar,
                &staging.join(spawn_helper_name(platform.exe_name)),
            ] {
                if path.is_file() {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
                }
            }
        }
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

/// A cache entry is valid when the marker, the executable, its ripgrep
/// sidecar, and (on macOS) the spawn helper all exist.
fn cache_valid(cache_dir: &Path, platform: &Platform) -> bool {
    let exe = cache_dir.join(platform.exe_name);
    if !cache_dir.join(".complete").is_file() || !exe.is_file() {
        return false;
    }
    if !cache_dir.join(sidecar_name(platform.exe_name)).is_file() {
        return false;
    }
    if is_macos(platform.exe_name)
        && !cache_dir
            .join(spawn_helper_name(platform.exe_name))
            .is_file()
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::to_pypi_version;

    #[test]
    fn normalizes_pre_release_spelling() {
        assert_eq!(to_pypi_version("0.1.0-rc.6"), "0.1.0rc6");
        assert_eq!(to_pypi_version("0.2.1-rc.2"), "0.2.1rc2");
        assert_eq!(to_pypi_version("0.2.0-beta.1"), "0.2.0b1");
        assert_eq!(to_pypi_version("0.3.0-alpha.3"), "0.3.0a3");
    }

    #[test]
    fn leaves_stable_and_pep440_versions_untouched() {
        assert_eq!(to_pypi_version("0.1.0"), "0.1.0");
        assert_eq!(to_pypi_version("0.1.0rc6"), "0.1.0rc6");
    }
}
