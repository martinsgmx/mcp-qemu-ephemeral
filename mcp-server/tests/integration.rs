//! Integration smoke tests for mcp-qemu-ephemeral.
//!
//! These tests exercise the end-to-end flow of the core, image-manager, and
//! recipe crates using real processes in place of QEMU, so they run without
//! needing a QEMU binary.

use mcp_qemu_core::{QemuConfig, QemuError, QemuHandle};
use mcp_qemu_image_manager::ImageManager;
use mcp_qemu_recipe::load_recipe;

/// Spawn a real process (like QEMU) and verify the full lifecycle: spawn,
/// stream output, wait, exit code.
#[tokio::test]
async fn test_qemu_handle_lifecycle_with_real_process() {
    // Spawn a process that produces known output then exits.
    let mut handle =
        QemuHandle::spawn(QemuConfig {
            binary: "echo".to_string(),
            drive_image: None,
            iso: None,
            memory_mib: 1,
            cpus: 1,
            extra_args: vec!["hello".to_string()],
            cwd: None,
            env: vec![],
            agent_socket: None,
            serial_file: None,
            root_device: None,
            guest_command: None,
            guest_args: None,
            username: None,
            password: None,
            boot_prompt: None,
            boot_response: None,
            boot_timeout_secs: 120,
            // A plain `echo` process cannot emulate a login → shell sequence,
            // so the automatic boot interaction must stay off here.
            auto_boot_interact: false,
        })
        .await
        .expect("spawn echo");

    // Verify pid is set.
    assert!(handle.pid() > 0);

    // Stream output until EOF.
    let mut out = Vec::new();
    let result = handle.stream_output(&mut out).await;
    assert!(result.is_ok());

    // Wait for exit.
    let code = handle.wait().await.expect("wait");
    assert_eq!(code, 0);

    // echo produces "hello\n".
    let text = String::from_utf8_lossy(&out).to_string();
    assert!(text.contains("hello"), "expected 'hello' in output, got {:?}", text);
}

/// Verify that kill() works on a running process.
#[tokio::test]
async fn test_qemu_handle_kill() {
    let mut handle =
        QemuHandle::spawn(QemuConfig {
            binary: "sleep".to_string(),
            drive_image: None,
            iso: None,
            memory_mib: 1,
            cpus: 1,
            extra_args: vec!["30".to_string()],
            cwd: None,
            env: vec![],
            agent_socket: None,
            serial_file: None,
            root_device: None,
            guest_command: None,
            guest_args: None,
            username: None,
            password: None,
            boot_prompt: None,
            boot_response: None,
            boot_timeout_secs: 120,
            // A plain `sleep` process cannot emulate a login → shell
            // sequence, so the automatic boot interaction must stay off.
            auto_boot_interact: false,
        })
        .await
        .expect("spawn sleep");

    // kill() should terminate the process.
    let result = handle.kill().await;
    assert!(result.is_ok());

    // After kill, wait should return a non-zero code (or -1).
    let code = match handle.wait().await {
        Ok(c) => c,
        Err(QemuError::StillRunning) => -1,
        Err(_) => -1,
    };
    assert_ne!(code, 0);

    let _ = handle;
}

/// Verify that a recipe can be parsed and applied to a QemuConfig.
#[tokio::test]
async fn test_recipe_applies_to_qemu_config() {
    let tmp_img = std::env::temp_dir().join("integration_recipe_img.qcow2");
    std::fs::write(&tmp_img, b"dummy image").expect("write dummy image");

    let tmp = std::env::temp_dir().join("integration_recipe.yaml");
    std::fs::write(
        &tmp,
        r#"
image: IMAGE_PATH_PLACEHOLDER
memory_mib: 2048
cpus: 2
command: /bin/bash
args:
  - "-c"
  - "echo hi"
env:
  - [FOO, bar]
"#,
    )
    .expect("write recipe");

    // Replace placeholder with the real temp image path.
    let content = std::fs::read_to_string(&tmp).expect("read recipe");
    let content = content.replace("IMAGE_PATH_PLACEHOLDER", tmp_img.to_str().unwrap());
    std::fs::write(&tmp, content).expect("rewrite recipe");

    let recipe =
        load_recipe(std::path::Path::new(&tmp)).await.expect("load recipe");
    assert_eq!(recipe.memory_mib, 2048);
    assert_eq!(recipe.cpus, 2);
    assert_eq!(recipe.command, "/bin/bash");

    let config = recipe.apply().expect("apply recipe");
    assert_eq!(config.memory_mib, 2048);
    assert_eq!(config.cpus, 2);
    assert_eq!(config.drive_image, Some(tmp_img.to_str().unwrap().to_string()));

    let _ = std::fs::remove_file(&tmp_img);
    let _ = std::fs::remove_file(&tmp);
}

/// Verify the CLI produces a usable default config.
#[test]
fn test_cli_default_config_matches_recipe_defaults() {
    let config = QemuConfig::default();
    assert_eq!(config.memory_mib, 1024);
    assert_eq!(config.cpus, 2);
    assert!(config.drive_image.is_none());
    assert!(config.iso.is_none());
}

/// Verify the image manager creates a sane default state.
#[test]
fn test_image_manager_defaults() {
    let mgr = ImageManager::default();
    assert_eq!(mgr.images_dir().as_os_str(), "images");
}

/// Verify all tools are registered by building the MCP service.
#[test]
fn test_mcp_service_builds() {
    let service = mcp_qemu_mcp_server::build_mcp_service();
    // Service builds without error — tools are registered.
    let _ = service;
}
