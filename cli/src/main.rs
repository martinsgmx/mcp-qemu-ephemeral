use std::io::Write;
use std::process::exit;

use clap::Parser;
use mcp_qemu_core::QemuConfig;

/// CLI entry point for `mcp-qemu-ephemeral`.
///
/// Dual-mode: this binary is the CLI. The MCP server lives in the `mcp-server`
/// crate and exposes the same core capabilities as tools.
#[derive(Parser)]
#[command(name = "mcp-qemu-ephemeral")]
#[command(about = "Manage isolated QEMU VMs: lifecycle, images, recipes, logs.")]
enum Cli {
    /// Run a script inside an isolated QEMU VM.
    ///
    /// Spawns a QEMU VM with the default config and the given script passed
    /// as an extra QEMU argument (so it appears in the VM's serial console
    /// and can be referenced by the guest), streams the VM's serial console
    /// output to stdout, and tears down the VM cleanly. The process exit code
    /// is the exit code of QEMU.
    Run {
        /// Path to the script to execute inside the VM.
        script: String,
    },
    /// Image management subcommands (stub).
    Image,
    /// Recipe management subcommands (stub).
    Recipe,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli {
        Cli::Run { script } => {
            let script_path = std::path::Path::new(&script);
            if !script_path.exists() {
                eprintln!("error: script not found: {script}");
                exit(1);
            }

            // Build a QEMU config that passes the script to the guest and
            // enables the Guest Agent socket so logs can be retrieved later.
            let mut config = QemuConfig::default();
            config.drive_image = None;
            config.iso = None;
            config.memory_mib = 1024;
            config.cpus = 2;
            // Pass the script path to QEMU so it is visible to the guest.
            config.extra_args = vec![
                "-device".to_string(),
                "virtio-9p,name=host,tag=mnt".to_string(),
                script,
            ];
            config.cwd = None;
            config.env = vec![];
            config.agent_socket = Some(std::path::PathBuf::from("/run/qemu/qga.sock"));
            config.root_device = Some("/dev/vda1".to_string());
            config.guest_command = Some("/bin/bash".to_string());
            config.guest_args = Some(vec!["-c".to_string()]);

            // Spawn the VM.
            let mut handle = mcp_qemu_core::QemuHandle::spawn(config).await?;

            // Stream all output (serial console + diagnostics) to stdout.
            let mut out = Vec::new();
            let result = handle.stream_output(&mut out).await;
            if let Err(e) = result {
                eprintln!("error streaming output: {e}");
            }

            // Wait for the VM to exit and return its exit code.
            let code = match handle.await_exit().await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error waiting for exit: {e}");
                    1
                }
            };

            // Print any captured output.
            if !out.is_empty() {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&out);
            }

            if code != 0 {
                exit(code);
            }
            Ok(())
        }
        Cli::Image => {
            println!("image subcommand not yet implemented");
            exit(0);
        }
        Cli::Recipe => {
            println!("recipe subcommand not yet implemented");
            exit(0);
        }
    }
}
