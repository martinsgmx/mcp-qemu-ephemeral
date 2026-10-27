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
    /// Test a script inside an ephemeral VM for a known distro.
    ///
    /// The high-level flow: fetch the distro ISO if needed, boot it as a
    /// live ISO with automatic serial-console login, run the script under
    /// the distro's shell, capture output/failures, tear down, and write a
    /// report into `runs/<run_id>/`. Prints the report to stdout.
    Test {
        /// Distro name from the profile registry (e.g. "alpine").
        #[arg(long, default_value = "alpine")]
        distro: String,
        /// ISO variant (e.g. "virt", "standard").
        #[arg(long)]
        variant: Option<String>,
        /// Guest architecture ("x86_64" or "aarch64").
        #[arg(long)]
        arch: Option<String>,
        /// Overall script timeout in seconds.
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// Path to the script to run inside the VM.
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
        Cli::Test {
            distro,
            variant,
            arch,
            timeout_secs,
            script,
        } => {
            let script_text =
                std::fs::read_to_string(&script).unwrap_or_else(|e| {
                    eprintln!("error: cannot read script {script}: {e}");
                    exit(1);
                });
            let arch = match arch.as_deref() {
                Some(a) => match a.parse() {
                    Ok(arch) => Some(arch),
                    Err(e) => {
                        eprintln!("error: {e}");
                        exit(1);
                    }
                },
                None => None,
            };
            let request = mcp_qemu_runner::RunRequest {
                distro,
                variant,
                arch,
                script: script_text,
                memory_mib: None,
                cpus: None,
                timeout_secs,
                boot_timeout_secs: None,
            };
            let report = mcp_qemu_runner::run(
                request,
                mcp_qemu_runner::RunPaths {
                    images_dir: mcp_qemu_image_manager::DEFAULT_IMAGES_DIR.into(),
                    runs_dir: "runs".into(),
                },
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("error: {e}");
                exit(1);
            });

            // Print the report and mirror the script's outcome.
            let md = std::fs::read_to_string(&report.report_path).unwrap_or_default();
            print!("{md}");
            if report.status.is_success() {
                println!("run_id: {}", report.run_id);
                Ok(())
            } else {
                println!("run_id: {} (status: {})", report.run_id, report.status);
                exit(1)
            }
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
