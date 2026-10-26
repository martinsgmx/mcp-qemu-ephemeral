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
