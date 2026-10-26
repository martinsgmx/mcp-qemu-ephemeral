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

/// Result type alias using `RecipeError`.
pub type Result<T> = std::result::Result<T, RecipeError>;

/// A parsed recipe describing a VM's configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Recipe {
    /// Path to the bootable disk image.
    pub image: PathBuf,
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

    /// Validate the recipe: image exists, memory/CPUs in sane bounds.
    pub fn validate(&self) -> Result<()> {
        if !self.image.exists() {
            return Err(RecipeError::ImageNotFound {
                path: self.image.clone(),
            });
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
        Ok(mcp_qemu_core::QemuConfig {
            binary: "qemu-system-x86_64".to_string(),
            drive_image: Some(self.image.to_string_lossy().into_owned()),
            iso: None,
            memory_mib: self.memory_mib,
            cpus: self.cpus,
            extra_args: self.qemu_args,
            cwd: None,
            env: self.env,
            // Root device: use the first partition of the drive image.
            root_device: Some("/dev/vda1".to_string()),
            // Run the recipe's command as the guest kernel's `init`.
            guest_command: Some(self.command.clone()),
            guest_args: Some(self.args.clone()),
            agent_socket: None,
            serial_file: None,
            username: None,
            password: None,
            boot_timeout_secs: 120,
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
            image: PathBuf::from("/data/images/ubuntu.qcow2"),
            memory_mib: 2048,
            cpus: 2,
            command: "/bin/bash".to_string(),
            args: vec!["-c".to_string(), "echo hi".to_string()],
            env: vec![("FOO".to_string(), "bar".to_string())],
            qemu_args: Vec::new(),
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
            image: tmp.clone(),
            memory_mib: 0,
            cpus: 2,
            command: "/bin/bash".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            qemu_args: Vec::new(),
        };
        let err = recipe.validate().unwrap_err();
        assert!(matches!(err, RecipeError::Invalid { .. }));
        let _ = std::fs::remove_file(&tmp);
    }
}
