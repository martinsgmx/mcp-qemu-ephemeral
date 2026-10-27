//! Recipe system for QEMU VMs.
//!
//! Recipes are YAML/JSON documents that describe a VM's configuration:
//! image, memory, CPUs, command, environment, and so on.
//!
//! ## Design notes
//!
//! - A recipe is a declarative document that can be loaded from YAML or JSON.
//! - It carries enough information to build a [`mcp_qemu_core::QemuConfig`]
//!   (via [`Recipe::apply`]) so the same recipe can be applied by both the
//!   CLI and the MCP server.
//! - Validation ensures the image path exists and memory/CPUs are in sane
//!   bounds before a VM is spawned.
//!
//! ## Example recipe (YAML)
//!
//! ```yaml
//! image: /data/images/ubuntu.qcow2
//! memory_mib: 2048
//! cpus: 2
//! command: /bin/bash
//! args:
//!   - -c
//!   - "echo hello"
//! env:
//!     FOO: bar
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Errors that can occur while managing recipes.
#[derive(Debug, thiserror::Error)]
pub enum RecipeError {
    /// Failed to parse the recipe document.
    #[error("failed to parse recipe: {0}")]
    Parse(String),
    /// The recipe is invalid.
    #[error("invalid recipe: {0}")]
    Invalid(String),
    /// The referenced image does not exist.
    #[error("image not found: {path}")]
    ImageNotFound { path: PathBuf },
    /// An I/O error reading the recipe file.
    #[error("io error: {0}")]
    Io(#[source] std::io::Error),
}

impl From<std::io::Error> for RecipeError {
    fn from(e: std::io::Error) -> Self {
        RecipeError::Io(e)
    }
}

impl From<serde_yaml::Error> for RecipeError {
    fn from(e: serde_yaml::Error) -> Self {
        RecipeError::Parse(e.to_string())
    }
}

/// Result type alias using `RecipeError`.
pub type Result<T> = std::result::Result<T, RecipeError>;

/// A parsed recipe describing a VM's configuration.
///
/// A recipe must reference at least one of `image` (a bootable disk) or
/// `iso` (a CD-ROM image). Live-ISO workflows use `iso` only.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Recipe {
    /// Path to the bootable disk image (optional when `iso` is set).
    #[serde(default)]
    pub image: Option<PathBuf>,
    /// Path to an ISO to attach as a CD-ROM (live-ISO workflows).
    #[serde(default)]
    pub iso: Option<PathBuf>,
    /// Guest architecture (`x86_64` or `aarch64`); selects the QEMU binary.
    #[serde(default)]
    pub arch: Option<String>,
    /// Memory in MiB.
    pub memory_mib: u64,
    /// Number of CPUs.
    pub cpus: u64,
    /// Command to run inside the VM.
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to pass to the command.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Optional extra QEMU arguments.
    #[serde(default)]
    pub qemu_args: Vec<String>,
    /// Username for the serial-console boot interaction.
    #[serde(default)]
    pub username: Option<String>,
    /// Password for the serial-console boot interaction (empty = Enter).
    #[serde(default)]
    pub password: Option<String>,
    /// Boot-loader prompt marker to answer (e.g. ISOLINUX `boot:`).
    #[serde(default)]
    pub boot_prompt: Option<String>,
    /// Line to send at the boot-loader prompt.
    #[serde(default)]
    pub boot_response: Option<String>,
    /// Boot timeout in seconds (default 120).
    #[serde(default)]
    pub boot_timeout_secs: Option<u64>,
}

impl Recipe {
    /// Parse a recipe from a YAML document.
    pub fn from_yaml(doc: &str) -> Result<Self> {
        serde_yaml::from_str(doc).map_err(|e| RecipeError::Parse(e.to_string()))
    }

    /// Parse a recipe from a JSON document.
    pub fn from_json(doc: &str) -> Result<Self> {
        serde_json::from_str(doc).map_err(|e| RecipeError::Parse(e.to_string()))
    }

    /// Validate the recipe: an image and/or iso exists, memory/CPUs in sane
    /// bounds, non-empty command.
    pub fn validate(&self) -> Result<()> {
        if self.image.is_none() && self.iso.is_none() {
            return Err(RecipeError::Invalid(
                "either image or iso is required".to_string(),
            ));
        }
        if let Some(image) = &self.image {
            if !image.exists() {
                return Err(RecipeError::ImageNotFound {
                    path: image.clone(),
                });
            }
        }
        if self.memory_mib == 0 || self.memory_mib > 1_048_576 {
            // 1 GiB max.
            return Err(RecipeError::Invalid(format!(
                "memory_mib {} is out of range (1..=1048576)",
                self.memory_mib
            )));
        }
        if self.cpus == 0 || self.cpus > 64 {
            return Err(RecipeError::Invalid(format!(
                "cpus {} is out of range (1..=64)",
                self.cpus
            )));
        }
        if self.command.is_empty() {
            return Err(RecipeError::Invalid("command is required".to_string()));
        }
        Ok(())
    }

    /// Apply the recipe to build a QEMU config.
    pub fn apply(self) -> Result<mcp_qemu_core::QemuConfig> {
        self.validate()?;
        // The QEMU binary follows the declared architecture.
        let binary = self
            .arch
            .as_deref()
            .map(|a| {
                mcp_qemu_profiles::Arch::from_str(a)
                    .map(|arch| arch.qemu_binary().to_string())
                    .map_err(|e| RecipeError::Invalid(e))
            })
            .transpose()?
            .unwrap_or_else(|| "qemu-system-x86_64".to_string());
        // Architecture machine-model arguments (AArch64 needs `-M virt`).
        let arch_machine_args = self
            .arch
            .as_deref()
            .and_then(|a| mcp_qemu_profiles::Arch::from_str(a).ok())
            .map(|arch| arch.qemu_machine_args().into_iter().map(String::from).collect())
            .unwrap_or_default();
        Ok(mcp_qemu_core::QemuConfig {
            binary,
            drive_image: self
                .image
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            iso: self.iso.as_ref().map(|p| p.to_string_lossy().into_owned()),
            memory_mib: self.memory_mib,
            cpus: self.cpus,
            // Machine args come before user-supplied qemu_args.
            extra_args: [arch_machine_args, self.qemu_args].concat(),
            cwd: None,
            env: self.env,
            // Root device: use the first partition of the drive image.
            root_device: Some("/dev/vda1".to_string()),
            // Run the recipe's command as the guest kernel's `init`.
            guest_command: Some(self.command.clone()),
            guest_args: Some(self.args.clone()),
            agent_socket: None,
            serial_file: None,
            username: self.username,
            password: self.password,
            boot_prompt: self.boot_prompt,
            boot_response: self.boot_response,
            boot_timeout_secs: self.boot_timeout_secs.unwrap_or(120),
            auto_boot_interact: true,
        })
    }
}

/// Load a recipe from a file path (detects YAML/JSON by extension).
pub async fn load_recipe(path: &Path) -> Result<Recipe> {
    let content = tokio::fs::read_to_string(path).await?;
    let recipe = if path
        .extension()
        .map_or(false, |e| e == "yaml" || e == "yml")
    {
        Recipe::from_yaml(&content)
    } else {
        Recipe::from_json(&content)
    };
    recipe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_recipe() -> Recipe {
        Recipe {
            image: Some(PathBuf::from("/data/images/ubuntu.qcow2")),
            iso: None,
            arch: None,
            memory_mib: 2048,
            cpus: 2,
            command: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), "echo hi".to_string()],
            env: vec![("FOO".to_string(), "bar".to_string())],
            qemu_args: Vec::new(),
            username: None,
            boot_prompt: None,
            boot_response: None,
            password: None,
            boot_timeout_secs: None,
        }
    }

    #[test]
    fn test_serialize_deserialize_yaml() {
        let recipe = sample_recipe();
        let yaml = serde_yaml::to_string(&recipe).unwrap();
        let parsed: Recipe = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.memory_mib, 2048);
        assert_eq!(parsed.cpus, 2);
    }

    #[test]
    fn test_from_yaml() {
        let doc = r#"
image: /data/images/ubuntu.qcow2
memory_mib: 2048
cpus: 2
command: /bin/bash
args:
  - "-c"
  - "echo hi"
env:
  - [FOO, bar]
"#;
        let recipe = Recipe::from_yaml(doc).unwrap();
        assert_eq!(recipe.memory_mib, 2048);
        assert_eq!(recipe.command, "/bin/bash");
        assert_eq!(recipe.env.len(), 1);
        assert_eq!(recipe.env[0].0, "FOO");
        assert_eq!(recipe.env[0].1, "bar");
    }

    #[test]
    fn test_from_json() {
        let doc = r#"{"image":"/data/images/ubuntu.qcow2","memory_mib":2048,"cpus":2,"command":"/bin/bash","args":["-c","echo hi"],"env":[["FOO","bar"]],"qemu_args":[]}"#;
        let recipe = Recipe::from_json(doc).unwrap();
        assert_eq!(recipe.memory_mib, 2048);
        assert_eq!(recipe.command, "/bin/bash");
        assert_eq!(recipe.env.len(), 1);
        assert_eq!(recipe.env[0].0, "FOO");
        assert_eq!(recipe.env[0].1, "bar");
    }

    #[test]
    fn test_validate_missing_image() {
        let recipe = sample_recipe();
        let err = recipe.validate().unwrap_err();
        assert!(matches!(err, RecipeError::ImageNotFound { .. }));
    }

    #[test]
    fn test_validate_bad_memory() {
        // Use a temp file as the image so the image check passes.
        let tmp = std::env::temp_dir().join("recipe_test_img.qcow2");
        std::fs::write(&tmp, b"dummy").unwrap();
        let recipe = Recipe {
            image: Some(tmp.clone()),
            iso: None,
            arch: None,
            memory_mib: 0,
            cpus: 2,
            command: "/bin/bash".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            qemu_args: Vec::new(),
            username: None,
            boot_prompt: None,
            boot_response: None,
            password: None,
            boot_timeout_secs: None,
        };
        let err = recipe.validate().unwrap_err();
        assert!(matches!(err, RecipeError::Invalid { .. }));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_validate_requires_image_or_iso() {
        let recipe = Recipe {
            image: None,
            iso: None,
            arch: None,
            memory_mib: 1024,
            cpus: 2,
            command: "/bin/sh".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            qemu_args: Vec::new(),
            username: None,
            boot_prompt: None,
            boot_response: None,
            password: None,
            boot_timeout_secs: None,
        };
        let err = recipe.validate().unwrap_err();
        assert!(matches!(err, RecipeError::Invalid { .. }));
    }

    #[test]
    fn test_iso_only_recipe_picks_arch_binary() {
        // The image does not need to exist for an iso-only recipe.
        let recipe = Recipe {
            image: None,
            iso: Some(PathBuf::from("/tmp/whatever-does-not-exist.iso")),
            arch: Some("aarch64".to_string()),
            memory_mib: 1024,
            cpus: 2,
            command: "/bin/sh".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            qemu_args: Vec::new(),
            username: Some("root".to_string()),
            boot_prompt: None,
            boot_response: None,
            password: Some("".to_string()),
            boot_timeout_secs: Some(240),
        };
        let config = recipe.apply().unwrap();
        assert_eq!(config.binary, "qemu-system-aarch64");
        assert!(config.drive_image.is_none());
        assert!(config.iso.is_some());
        // AArch64 machine args were injected ahead of user args.
        assert_eq!(
            config.extra_args,
            vec![
                "-M".to_string(),
                "virt".to_string(),
                "-cpu".to_string(),
                "cortex-a57".to_string()
            ]
        );
        assert_eq!(config.username.as_deref(), Some("root"));
        assert_eq!(config.password.as_deref(), Some(""));
        assert_eq!(config.boot_timeout_secs, 240);
    }
}
