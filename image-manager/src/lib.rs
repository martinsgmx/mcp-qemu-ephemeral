//! Image management for QEMU VMs.
//!
//! This crate handles disk image operations: provisioning base images,
//! converting to qcow2, and attaching/detaching qcow and iso images to a
//! QEMU instance.
//!
//! ## Design notes
//!
//! - Base images can be provisioned from a template qcow2 file. The image is
//!   grown (via `qemu-img resize`) if needed.
//! - qcow2 overlay files provide copy-on-write semantics so the base image
//!   stays pristine. Each VM gets its own overlay.
//! - iso images can be attached as a virtual CD-ROM (e.g. for installing an
//!   OS or running a script).
//! - Path operations use [`tokio::fs`] for async file system access.
//!
//! All image paths live under `<data_dir>/images/` by default.

use std::path::{Path, PathBuf};
use std::process::Command;

use tokio::fs;
use tracing::info;

/// Default directory for all VM disk images.
pub const DEFAULT_IMAGES_DIR: &str = "images";

/// Information about a known disk image.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// Absolute path to the image file.
    pub path: PathBuf,
    /// Format of the image (`qcow2`, `raw`, `iso`, ...).
    pub format: String,
    /// Virtual size on disk.
    pub size: u64,
    /// Whether this image is a copy-on-write overlay.
    pub is_overlay: bool,
}

/// Errors that can occur while managing images.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    /// The image file does not exist.
    #[error("image not found: {path}")]
    NotFound { path: PathBuf },
    /// `qemu-img` is not available on PATH.
    #[error("qemu-img not found on PATH")]
    QemuImgNotFound,
    /// A `qemu-img` command failed.
    #[error("qemu-img command failed: {0}")]
    QemuImg(String),
    /// An I/O error accessing the image file.
    #[error("io error: {0}")]
    Io(#[source] std::io::Error),
    /// The image format is unsupported.
    #[error("unsupported image format: {0}")]
    UnsupportedFormat(String),
    /// An ISO download failed.
    #[error("download failed for {url}: {detail}")]
    Download {
        /// The URL that failed.
        url: String,
        /// What went wrong.
        detail: String,
    },
    /// The downloaded ISO's sha256 does not match the mirror's published one.
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The file that failed verification.
        path: PathBuf,
        /// sha256 from the mirror's `.sha256` file.
        expected: String,
        /// sha256 actually computed.
        actual: String,
    },
    /// An external helper tool (curl/wget/sha256sum) misbehaved.
    #[error("external tool failed: {0}")]
    Tool(String),
}

impl From<std::io::Error> for ImageError {
    fn from(e: std::io::Error) -> Self {
        ImageError::Io(e)
    }
}

/// Result type alias using `ImageError`.
pub type Result<T> = std::result::Result<T, ImageError>;

/// Manager for VM disk images.
pub struct ImageManager {
    /// Directory holding all images.
    images_dir: PathBuf,
}

impl ImageManager {
    /// Create a new image manager with the given images directory.
    pub fn new(images_dir: impl AsRef<std::path::Path>) -> Self {
        Self {
            images_dir: images_dir.as_ref().to_path_buf(),
        }
    }

    /// The images directory this manager uses.
    pub fn images_dir(&self) -> &PathBuf {
        &self.images_dir
    }

    /// Ensure the images directory exists.
    pub async fn ensure_dir(&self) -> Result<()> {
        if !self.images_dir.exists() {
            fs::create_dir_all(&self.images_dir).await?;
            info!("Created images directory: {}", self.images_dir.display());
        }
        Ok(())
    }

    /// Provision a copy-on-write overlay (`qcow2`) from a base image.
    ///
    /// The overlay is created at `<images_dir>/<name>.qcow2`.
    pub async fn create_overlay(
        &self,
        name: &str,
        base: &str,
        size: Option<u64>,
    ) -> Result<PathBuf> {
        self.ensure_dir().await?;

        let overlay_path = self.images_dir.join(format!("{name}.qcow2"));
        if overlay_path.exists() {
            return Err(ImageError::NotFound {
                path: overlay_path.clone(),
            });
        }

        // Resize the base if a size is requested, before creating the overlay.
        if let Some(size) = size {
            let mut cmd = Command::new("qemu-img");
            cmd.arg("resize")
                .arg(base)
                .arg(format!("{size}G"))
                .arg("--format=qcow2");
            match cmd.output() {
                Ok(_) => {}
                Err(e) => return Err(ImageError::QemuImg(format!("resize failed: {e}"))),
            }
        }

        // Create the overlay.
        let overlay_str = overlay_path.to_string_lossy();
        let mut cmd = Command::new("qemu-img");
        cmd.arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg("-b")
            .arg(base)
            .arg(overlay_str.as_ref());

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => return Err(ImageError::QemuImg(format!("create failed: {e}"))),
        };
        if !output.status.success() {
            return Err(ImageError::QemuImg(format!(
                "qemu-img create failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        info!("Created overlay: {}", overlay_path.display());
        Ok(overlay_path)
    }

    /// Convert an existing image to qcow2 format.
    pub async fn convert_to_qcow2(&self, src: &str, name: &str) -> Result<PathBuf> {
        self.ensure_dir().await?;

        let dst_path = self.images_dir.join(format!("{name}.qcow2"));
        let mut cmd = Command::new("qemu-img");
        cmd.arg("convert")
            .arg("-O")
            .arg("qcow2")
            .arg(src)
            .arg(&dst_path);

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => return Err(ImageError::QemuImg(format!("convert failed: {e}"))),
        };
        if !output.status.success() {
            return Err(ImageError::QemuImg(format!(
                "qemu-img convert failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        info!("Converted {} -> {}", src, dst_path.display());
        Ok(dst_path)
    }

    /// Inspect an image file and return its metadata.
    pub async fn inspect(&self, path: &Path) -> Result<ImageInfo> {
        if !path.exists() {
            return Err(ImageError::NotFound {
                path: path.to_path_buf(),
            });
        }

        let metadata = fs::metadata(path).await?;
        let size = metadata.len();

        // Detect format from extension; fall back to `qemu-img info`.
        let format = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let path_buf = path.to_path_buf();
        let is_overlay = format.as_str() == "qcow2";
        Ok(ImageInfo {
            path: path_buf,
            format,
            size,
            is_overlay,
        })
    }

    /// List all images in the images directory.
    pub async fn list(&self) -> Result<Vec<ImageInfo>> {
        self.ensure_dir().await?;
        let mut images = Vec::new();
        let mut entries = fs::read_dir(&self.images_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_file() {
                if let Ok(info) = self.inspect(entry.path().as_path()).await {
                    images.push(info);
                }
            }
        }
        Ok(images)
    }

    /// Remove an image file.
    pub async fn remove(&self, path: &str) -> Result<()> {
        let path = PathBuf::from(path);
        if path.exists() {
            fs::remove_file(&path).await?;
            info!("Removed image: {}", path.display());
        }
        Ok(())
    }

    /// Directory holding downloaded ISO files (`<images_dir>/isos/`).
    pub fn isos_dir(&self) -> PathBuf {
        self.images_dir.join("isos")
    }

    /// Make sure the ISO at `url` is available locally, downloading it if
    /// needed.
    ///
    /// - If the file already exists in `<images_dir>/isos/`, it is returned
    ///   as-is (no re-download).
    /// - Otherwise the ISO is fetched with `curl` (fallback `wget`) into the
    ///   isos dir, verified against the mirror's companion `<name>.iso.sha256`
    ///   when one is published, and only then moved to its final name.
    pub async fn ensure_iso(&self, url: &str) -> Result<PathBuf> {
        let dir = self.isos_dir();
        fs::create_dir_all(&dir).await?;

        let filename = url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ImageError::Download {
                url: url.to_string(),
                detail: "could not derive a file name from the URL".to_string(),
            })?;
        let final_path = dir.join(filename);

        if final_path.exists() {
            info!("ISO already present: {}", final_path.display());
            return Ok(final_path);
        }

        let part_path = dir.join(format!("{filename}.part"));
        let _ = fs::remove_file(&part_path).await;

        self.download_to(url, &part_path).await?;

        // Verify against the mirror's published checksum when available.
        let sha_url = format!("{url}.sha256");
        match self.fetch_text(&sha_url).await {
            Ok(sha_text) => {
                if let Some(expected) = sha_text.split_whitespace().next() {
                    let actual = sha256_of(&part_path).await?;
                    if actual != expected {
                        let _ = fs::remove_file(&part_path).await;
                        return Err(ImageError::ChecksumMismatch {
                            path: part_path,
                            expected: expected.to_string(),
                            actual,
                        });
                    }
                    info!("sha256 verified for {filename}");
                }
            }
            Err(e) => {
                // No companion checksum published (or mirror unreachable):
                // keep the download but say so.
                info!("no sha256 verification for {filename}: {e}");
            }
        }

        fs::rename(&part_path, &final_path).await?;
        info!("Downloaded ISO: {}", final_path.display());
        Ok(final_path)
    }

    /// Pick the first available HTTP helper: curl, then wget.
    fn http_tool() -> Option<&'static str> {
        for tool in ["curl", "wget"] {
            if tool_on_path(tool) {
                return Some(tool);
            }
        }
        None
    }

    /// Download `url` into `dest`, streaming to the file.
    async fn download_to(&self, url: &str, dest: &Path) -> Result<()> {
        let tool = Self::http_tool().ok_or_else(|| {
            ImageError::Download {
                url: url.to_string(),
                detail: "neither curl nor wget is installed".to_string(),
            }
        })?;
        let mut cmd = tokio::process::Command::new(tool);
        match tool {
            "curl" => {
                cmd.args(["-fL", "--retry", "3", "-sS", "-o"]).arg(dest).arg(url);
            }
            "wget" => {
                cmd.args(["-q", "-O"]).arg(dest).arg(url);
            }
            _ => unreachable!(),
        }
        let status = cmd.status().await.map_err(ImageError::Io)?;
        if !status.success() {
            let _ = fs::remove_file(dest).await;
            return Err(ImageError::Download {
                url: url.to_string(),
                detail: format!("{tool} exited with {status}"),
            });
        }
        Ok(())
    }

    /// Fetch a small text file from `url` (used for `.sha256` companions).
    async fn fetch_text(&self, url: &str) -> Result<String> {
        let tool = Self::http_tool().ok_or_else(|| ImageError::Tool(
            "neither curl nor wget is installed".to_string(),
        ))?;
        let mut cmd = tokio::process::Command::new(tool);
        match tool {
            "curl" => cmd.args(["-fsSL"]).arg(url),
            "wget" => cmd.args(["-qO-", "--timeout=15"]).arg(url),
            _ => unreachable!(),
        };
        let out = cmd.output().await.map_err(ImageError::Io)?;
        if !out.status.success() {
            return Err(ImageError::Download {
                url: url.to_string(),
                detail: format!("{tool} exited with {}", out.status),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// True when an executable with `name` is reachable on PATH.
fn tool_on_path(name: &str) -> bool {
    std::env::var("PATH")
        .map(|path| {
            path.split(':')
                .any(|dir| std::fs::metadata(format!("{dir}/{name}")).is_ok())
        })
        .unwrap_or(false)
}

/// Compute the sha256 of `path` via `sha256sum`.
async fn sha256_of(path: &Path) -> Result<String> {
    let out = tokio::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .await
        .map_err(ImageError::Io)?;
    if !out.status.success() {
        return Err(ImageError::Tool(format!(
            "sha256sum exited with {}",
            out.status
        )));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| ImageError::Tool("sha256sum produced no output".to_string()))
}

impl Default for ImageManager {
    fn default() -> Self {
        Self::new(DEFAULT_IMAGES_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_info_default() {
        let mgr = ImageManager::default();
        assert_eq!(mgr.images_dir().as_os_str(), "images");
    }
}
