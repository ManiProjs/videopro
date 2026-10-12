//! On-demand, content-versioned proxy generation for source media.
//!
//! Proxies live in the operating system's application cache and never replace
//! the source file. This module deliberately has no UI or project-file coupling.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Result of obtaining a usable H.264 proxy for a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyMedia {
    /// Canonical path of the original media.
    pub source_path: PathBuf,
    /// Path of the cached H.264 MP4 proxy.
    pub proxy_path: PathBuf,
    /// Stable key derived from source path, size, and modification time.
    pub cache_key: String,
    /// True when a previously generated valid proxy was reused.
    pub reused: bool,
}

/// Errors encountered while locating, validating, or generating a proxy.
#[derive(Debug)]
pub struct ProxyError(String);

impl ProxyError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ProxyError {}

/// Return the platform-specific VideoPro cache directory.
pub fn cache_dir() -> Result<PathBuf, ProxyError> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Caches"))
        .ok_or_else(|| ProxyError::new("Cannot locate the user cache directory (HOME is unset)"))?;

    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| {
            ProxyError::new("Cannot locate the user cache directory (LOCALAPPDATA is unset)")
        })?;

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| {
            ProxyError::new(
                "Cannot locate the user cache directory (XDG_CACHE_HOME and HOME are unset)",
            )
        })?;

    Ok(base.join("VideoPro").join("proxies"))
}

/// Calculate the stable cache key for a source using its canonical path,
/// byte length, and modification timestamp. No media is read or generated.
pub fn cache_key_for(source: impl AsRef<Path>) -> Result<String, ProxyError> {
    let source = source.as_ref();
    let canonical = fs::canonicalize(source).map_err(|error| {
        ProxyError::new(format!(
            "Cannot resolve source media '{}': {error}",
            source.display()
        ))
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        ProxyError::new(format!(
            "Cannot read source metadata '{}': {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(ProxyError::new(format!(
            "Source media is not a regular file: {}",
            canonical.display()
        )));
    }
    let modified = metadata
        .modified()
        .map_err(|error| {
            ProxyError::new(format!(
                "Cannot read modification time for '{}': {error}",
                canonical.display()
            ))
        })?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            ProxyError::new(format!(
                "Invalid modification time for '{}': {error}",
                canonical.display()
            ))
        })?;

    let mut hash = StableFnv64::new();
    hash.write(canonical.to_string_lossy().as_bytes());
    hash.write(&[0]);
    hash.write(&metadata.len().to_le_bytes());
    hash.write(&modified.as_secs().to_le_bytes());
    hash.write(&modified.subsec_nanos().to_le_bytes());
    Ok(format!("{:016x}", hash.finish()))
}

/// Return the deterministic cache destination for a source file.
pub fn proxy_path_for(source: impl AsRef<Path>) -> Result<PathBuf, ProxyError> {
    Ok(cache_path_for_key(&cache_dir()?, &cache_key_for(source)?))
}

/// Generate a low-resolution H.264 MP4 proxy, or reuse a valid cached one.
/// Requires `ffmpeg` and `ffprobe` to be discoverable on `PATH`.
pub fn create_proxy(source: impl AsRef<Path>) -> Result<ProxyMedia, ProxyError> {
    let source_arg = source.as_ref();
    let source_path = fs::canonicalize(source_arg).map_err(|error| {
        ProxyError::new(format!(
            "Cannot resolve source media '{}': {error}",
            source_arg.display()
        ))
    })?;
    let metadata = fs::metadata(&source_path).map_err(|error| {
        ProxyError::new(format!(
            "Cannot read source media '{}': {error}",
            source_path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(ProxyError::new(format!(
            "Source media is not a regular file: {}",
            source_path.display()
        )));
    }
    let cache_key = cache_key_for(&source_path)?;
    let directory = cache_dir()?;
    let proxy_path = cache_path_for_key(&directory, &cache_key);
    if source_path == proxy_path {
        return Err(ProxyError::new(
            "Refusing to use the source media as its own proxy",
        ));
    }
    fs::create_dir_all(&directory).map_err(|error| {
        ProxyError::new(format!(
            "Cannot create proxy cache '{}': {error}",
            directory.display()
        ))
    })?;

    if is_valid_proxy(&proxy_path)? {
        return Ok(ProxyMedia {
            source_path,
            proxy_path,
            cache_key,
            reused: true,
        });
    }
    if proxy_path.exists() {
        fs::remove_file(&proxy_path).map_err(|error| {
            ProxyError::new(format!(
                "Cannot remove invalid cached proxy '{}': {error}",
                proxy_path.display()
            ))
        })?;
    }

    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".{cache_key}.{}.{}.tmp.mp4",
        std::process::id(),
        id
    ));
    let result = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&source_path)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-vf",
            "scale=960:540:force_original_aspect_ratio=decrease:force_divisible_by=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "28",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-movflags",
            "+faststart",
        ])
        .arg(&temporary)
        .output()
        .map_err(|error| {
            ProxyError::new(format!(
                "Could not start ffmpeg to generate proxy (is ffmpeg installed?): {error}"
            ))
        })?;

    if !result.status.success() {
        let _ = fs::remove_file(&temporary);
        let stderr = String::from_utf8_lossy(&result.stderr);
        let detail = stderr
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(ProxyError::new(format!(
            "FFmpeg could not generate a proxy for '{}': {}",
            source_path.display(),
            if detail.is_empty() {
                format!("process exited with {}", result.status)
            } else {
                detail
            }
        )));
    }

    match is_valid_proxy(&temporary) {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_file(&temporary);
            return Err(ProxyError::new(
                "FFmpeg completed but its output is not a valid H.264 video proxy",
            ));
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    fs::rename(&temporary, &proxy_path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        ProxyError::new(format!(
            "Cannot publish proxy to '{}': {error}",
            proxy_path.display()
        ))
    })?;

    Ok(ProxyMedia {
        source_path,
        proxy_path,
        cache_key,
        reused: false,
    })
}

fn cache_path_for_key(directory: &Path, key: &str) -> PathBuf {
    directory.join(format!("{key}.mp4"))
}

fn is_valid_proxy(path: &Path) -> Result<bool, ProxyError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() > 0 => {}
        _ => return Ok(false),
    }
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .map_err(|error| {
            ProxyError::new(format!(
                "Could not start ffprobe to validate proxy (is ffprobe installed?): {error}"
            ))
        })?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let fields = stdout
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .collect::<std::collections::HashMap<_, _>>();
    Ok(fields.get("codec_name") == Some(&"h264")
        && fields
            .get("width")
            .is_some_and(|value| value.parse::<u32>().is_ok_and(|v| v > 0))
        && fields
            .get("height")
            .is_some_and(|value| value.parse::<u32>().is_ok_and(|v| v > 0)))
}

// Fixed FNV-1a makes keys reproducible across process launches and independent
// of Rust's randomized hashers.
struct StableFnv64(u64);

impl StableFnv64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    fn new() -> Self {
        Self(Self::OFFSET)
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_is_repeatable_and_sensitive_to_metadata() {
        let hash = |path: &str, size: u64, seconds: u64, nanos: u32| {
            let mut hasher = StableFnv64::new();
            hasher.write(path.as_bytes());
            hasher.write(&[0]);
            hasher.write(&size.to_le_bytes());
            hasher.write(&seconds.to_le_bytes());
            hasher.write(&nanos.to_le_bytes());
            hasher.finish()
        };
        assert_eq!(
            hash("/media/clip.mov", 123, 456, 789),
            hash("/media/clip.mov", 123, 456, 789)
        );
        assert_ne!(
            hash("/media/clip.mov", 123, 456, 789),
            hash("/media/other.mov", 123, 456, 789)
        );
        assert_ne!(
            hash("/media/clip.mov", 123, 456, 789),
            hash("/media/clip.mov", 124, 456, 789)
        );
        assert_ne!(
            hash("/media/clip.mov", 123, 456, 789),
            hash("/media/clip.mov", 123, 457, 789)
        );
    }

    #[test]
    fn cache_path_is_deterministic_and_uses_mp4_extension() {
        let directory = Path::new("/tmp/videopro-proxies");
        let first = cache_path_for_key(directory, "0123456789abcdef");
        let second = cache_path_for_key(directory, "0123456789abcdef");
        assert_eq!(first, second);
        assert_eq!(first, directory.join("0123456789abcdef.mp4"));
    }
}
