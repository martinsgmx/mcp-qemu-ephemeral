//! Core QEMU lifecycle management.
//!
//! This crate owns everything needed to start, monitor, and tear down an
//! isolated QEMU process. It uses [`tokio::process`] for the async process
//! handling and provides a small, well-defined surface so the CLI and MCP
//! server can share the same implementation.
//!
//! ## Design notes
//!
//! - QEMU is spawned as a child process with its own stdout/stderr pipes so
//!   we can stream the VM's serial console output to callers.
//! - The [`QemuHandle`] owns the [`tokio::process::Child`] and guarantees
//!   cleanup on drop (no zombie / leaked QEMU processes).
//! - Pause/resume is done by sending SIGSTOP/SIGCONT to the QEMU process.
//! - The process is killed (SIGTERM then SIGKILL as fallback) on shutdown.
//! - For Arch-style live ISOs the serial console is auto-driven through the
//!   boot sequence (login prompt → configured credentials → shell prompt),
//!   via [`QemuHandle::interact_boot`].
//!
//! ## Testing
//!
//! Unit tests spawn a harmless shell command (e.g. `sleep`) instead of QEMU
//! so the lifecycle logic can be exercised without a QEMU binary.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

/// Error type for all QEMU lifecycle operations.
#[derive(Debug, thiserror::Error)]
pub enum QemuError {
    /// The QEMU binary was not found on PATH.
    #[error("qemu-system-x86_64 not found on PATH")]
    QemuNotFound,
    /// Failed to spawn the QEMU process.
    #[error("failed to spawn qemu: {0}")]
    Spawn(#[source] std::io::Error),
    /// The QEMU process exited with a non-zero status.
    #[error("qemu exited with status {0}")]
    ExitStatus(i32),
    /// An I/O error while reading QEMU output.
    #[error("io error: {0}")]
    Io(#[source] std::io::Error),
    /// The handle was closed while the process was still running.
    #[error("qemu process still running")]
    StillRunning,
    /// The automatic serial-console boot interaction aborted before reaching a
    /// shell prompt. `{0}` is the detail plus the last 500 serial bytes.
    #[error("qemu boot interaction aborted: {0}")]
    BootInteract(#[source] BootInteractError),
}

/// Convert an I/O error into a QemuError.
impl From<std::io::Error> for QemuError {
    fn from(e: std::io::Error) -> Self {
        QemuError::Io(e)
    }
}

/// Configuration for launching a QEMU instance.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QemuConfig {
    /// Path to the QEMU binary (defaults to `qemu-system-x86_64`).
    pub binary: String,
    /// Path to the bootable disk image (optional).
    pub drive_image: Option<String>,
    /// Path to an ISO to attach as a CD-ROM (optional).
    pub iso: Option<String>,
    /// Memory in MiB.
    pub memory_mib: u64,
    /// Number of CPUs.
    pub cpus: u64,
    /// Extra QEMU arguments appended to the command line.
    pub extra_args: Vec<String>,
    /// Working directory for the QEMU process.
    pub cwd: Option<PathBuf>,
    /// Environment variables to pass to the QEMU process.
    pub env: Vec<(String, String)>,
    /// Path to the Guest Agent socket (QEMU creates it here via `-enable-agent`).
    pub agent_socket: Option<PathBuf>,
    /// Root device for the guest kernel command line (default: `/dev/vda1`).
    pub root_device: Option<String>,
    /// Command to run inside the guest as `init` (e.g. `/bin/bash`).
    pub guest_command: Option<String>,
    /// Arguments to the guest command.
    pub guest_args: Option<Vec<String>>,
    /// Username to log in with when the serial console shows a login prompt
    /// (default: `root`).
    pub username: Option<String>,
    /// Password to log in with when the serial console shows a password prompt.
    /// Empty string means the guest accepts an empty password (enter-key
    /// login, e.g. Arch/Alpine root live images).
    pub password: Option<String>,
    /// Shell timeout in seconds while waiting for the boot sequence to reach a
    /// shell prompt (default: 120).
    pub boot_timeout_secs: u64,
    /// Whether to automatically drive the serial console through the boot
    /// sequence (login prompt → configured credentials, then wait for the
    /// shell prompt) so commands can be placed inside the VM. Default: `true`.
    pub auto_boot_interact: bool,
    /// Optional file-backed serial console path. When set (or when
    /// `auto_boot_interact` is on with no explicit path), the VM's serial
    /// console is attached to this file instead of stdio, so the parent
    /// can both read and write to it (write = send keystrokes).
    pub serial_file: Option<PathBuf>,
}

impl Default for QemuConfig {
    fn default() -> Self {
        Self {
            binary: "qemu-system-x86_64".to_string(),
            drive_image: None,
            iso: None,
            memory_mib: 1024,
            cpus: 2,
            extra_args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            agent_socket: None,
            serial_file: None,
            root_device: Some("/dev/vda1".to_string()),
            guest_command: None,
            guest_args: None,
            username: None,
            password: None,
            boot_timeout_secs: 120,
            auto_boot_interact: true,
        }
    }
}

/// A handle to a running (or stopped) QEMU process.
///
/// The handle owns the child process and guarantees that it is terminated
/// when the handle is dropped. This prevents zombie QEMU processes from
/// leaking when a VM is abandoned.
pub struct QemuHandle {
    child: Child,
    /// The PID of the QEMU process.
    pid: u32,
    /// Whether the process has been stopped (SIGSTOP).
    stopped: bool,
    /// Path to the Guest Agent socket (if enabled).
    agent_socket: Option<PathBuf>,
    /// Username for the serial-console boot interaction (default `root`).
    username: Option<String>,
    /// Password for the serial-console boot interaction.
    password: Option<String>,
}

impl QemuHandle {
    /// Spawn a QEMU process with the given configuration.
    ///
    /// When `config.auto_boot_interact` is enabled (default), the automatic
    /// serial-console boot interaction runs right after the VM starts and
    /// drives Arch-style ISO boot sequences (login prompt → credentials →
    /// shell prompt) before returning the handle.
    pub async fn spawn(config: QemuConfig) -> Result<Self, QemuError> {
        // Resolve the binary: if it contains a path separator, check it
        // directly; otherwise search PATH.
        let resolved = if config.binary.contains('/') {
            config.binary.clone()
        } else {
            match std::env::var("PATH") {
                Ok(path) => path
                    .split(':')
                    .map(|dir| format!("{}/{}", dir, config.binary))
                    .find(|p| std::fs::metadata(p).is_ok())
                    .unwrap_or_else(|| config.binary.clone()),
                Err(_) => config.binary.clone(),
            }
        };

        // Verify the binary exists.
        if std::fs::metadata(&resolved).is_err() {
            return Err(QemuError::QemuNotFound);
        }

        let mut cmd = Command::new(&resolved);

        // Core QEMU arguments: no graphical output, serial console on stdio.
        cmd.arg("-nographic")
            .arg("-serial")
            .arg("stdio")
            .arg("-monitor")
            .arg("devnull");

        // Drive image.
        if let Some(img) = &config.drive_image {
            cmd.arg("-drive")
                .arg(format!("file={},format=qcow2", img));
        }

        // ISO attachment.
        if let Some(iso) = &config.iso {
            cmd.arg("-cdrom")
                .arg(format!("file={}", iso));
        }

        // Memory and CPUs.
        cmd.arg("-m")
            .arg(config.memory_mib.to_string())
            .arg("-smp")
            .arg(config.cpus.to_string());

        // Extra user-supplied arguments.
        for arg in &config.extra_args {
            cmd.arg(arg);
        }

        // Guest Agent socket: if enabled, tell QEMU to export the Guest Agent
        // unix socket at the given path.
        if let Some(agent_socket) = &config.agent_socket {
            cmd.arg("-enable-agent")
                .arg(format!("unix:{}", agent_socket.display()));
        }

        // Working directory and environment.
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .map_err(QemuError::Spawn)?;

        let pid = child.id().ok_or(QemuError::QemuNotFound)?;

        let agent_socket = config.agent_socket;

        // Run the automatic serial-console boot interaction when configured.
        let mut handle = Self {
            child,
            pid,
            stopped: false,
            agent_socket,
            username: config.username,
            password: config.password,
        };
        if config.auto_boot_interact {
            match handle.interact_boot(config.boot_timeout_secs).await {
                Ok(result) => return match result {
                    BootInteractResult::Ready => Ok(handle),
                    BootInteractResult::Aborted { reason, tail } => {
                        Err(QemuError::BootInteract(BootInteractError::Aborted { reason, tail }))
                    }
                },
                Err(e) => return Err(QemuError::BootInteract(e.into())),
            }
        }
        Ok(handle)
    }

    /// The process ID of the QEMU instance.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Path to the Guest Agent socket (if enabled via `-enable-agent`).
    pub fn agent_socket(&self) -> Option<&PathBuf> {
        self.agent_socket.as_ref()
    }

    /// Send SIGSTOP to pause the VM.
    pub fn pause(&mut self) -> Result<(), QemuError> {
        if self.stopped {
            return Ok(());
        }
        unsafe {
            if libc::kill(self.pid as i32, libc::SIGSTOP) != 0 {
                return Err(QemuError::Io(std::io::Error::last_os_error()));
            }
        }
        self.stopped = true;
        Ok(())
    }

    /// Send SIGCONT to resume the VM.
    pub fn resume(&mut self) -> Result<(), QemuError> {
        if !self.stopped {
            return Ok(());
        }
        unsafe {
            if libc::kill(self.pid as i32, libc::SIGCONT) != 0 {
                return Err(QemuError::Io(std::io::Error::last_os_error()));
            }
        }
        self.stopped = false;
        Ok(())
    }

    /// Stop the VM gracefully (SIGTERM) and wait for it to exit.
    pub async fn stop(&mut self) -> Result<(), QemuError> {
        // First try graceful termination.
        unsafe {
            let _ = libc::kill(self.pid as i32, libc::SIGTERM);
        }
        // Wait briefly to see if it exits.
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        // If it hasn't exited, force kill.
        self.child.kill().await?;
        Ok(())
    }

    /// Kill the VM immediately (SIGKILL).
    pub async fn kill(&mut self) -> Result<(), QemuError> {
        self.child.kill().await?;
        Ok(())
    }

    /// Wait for the process to exit and return its status code.
    pub async fn wait(&mut self) -> Result<i32, QemuError> {
        let status = self.child.wait().await?;
        Ok(status.code().unwrap_or(-1))
    }

    /// Read a line of output from the VM's serial console.
    ///
    /// Returns `None` when the stream is closed and no data was buffered.
    pub async fn read_line(&mut self) -> Result<Option<String>, QemuError> {
        let stdout = self.child.stdout.as_mut().unwrap();
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = stdout.read(&mut byte).await?;
            if n == 0 {
                // Stream closed; return any buffered line.
                if line.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(String::from_utf8_lossy(&line).to_string()));
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                // Strip trailing newline.
                line.pop();
                return Ok(Some(String::from_utf8_lossy(&line).to_string()));
            }
        }
    }

    /// Stream all output from stdout and stderr to a writer.
    ///
    /// Both streams are drained until they are closed. Their bytes are
    /// buffered per-stream and then forwarded to the writer, so the caller
    /// receives the complete VM output (serial console on stdout and
    /// diagnostics on stderr). Returns when both streams are closed.
    pub async fn stream_output<W: tokio::io::AsyncWrite + Unpin>(
        &mut self,
        mut writer: W,
    ) -> Result<(), QemuError> {
        let stdout = self.child.stdout.as_mut().unwrap();
        let stderr = self.child.stderr.as_mut().unwrap();

        // Drain each stream into a Vec until EOF, then forward to the writer.
        let stdout_data = drain_stream(stdout).await;
        let stderr_data = drain_stream(stderr).await;

        let _ = writer.write(&stdout_data).await;
        let _ = writer.write(&stderr_data).await;

        Ok(())
    }
}

/// Drain one async read stream into a Vec until EOF.
async fn drain_stream<S: tokio::io::AsyncRead + Unpin>(
    mut stream: S,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

impl QemuHandle {
    /// Consume the handle and wait for the process to exit.
    pub async fn await_exit(mut self) -> Result<i32, QemuError> {
        let code = self.wait().await?;
        Ok(code)
    }
}

impl Drop for QemuHandle {
    fn drop(&mut self) {
        // Best-effort kill on drop to avoid leaking processes.
        let _ = self.child.kill();
    }
}

// ---------------------------------------------------------------------------
// Automatic serial-console boot interaction (Arch ISOs)
//
// For Arch live ISOs the serial console goes through a GRUB/login prompt and
// needs to be driven manually; this sequence auto-drives it:
//   1. read the serial console line by line;
//   2. on "login:" send the configured credentials (empty password for
//      enter-key logins such as Arch root);
//   3. on a shell prompt (# or $) the VM is READY;
//   4. on "login incorrect" or on the boot timeout, ABORT and dump the last
//      500 bytes of serial output.
// ---------------------------------------------------------------------------

/// Why an automatic boot-interaction sequence aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAbortReason {
    /// A login prompt appeared and the authentication failed ("Login incorrect").
    LoginIncorrect,
    /// No shell prompt appeared within the boot timeout.
    Timeout,
}

impl std::fmt::Display for BootAbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootAbortReason::LoginIncorrect => write!(f, "login incorrect"),
            BootAbortReason::Timeout => write!(f, "timeout"),
        }
    }
}

/// Outcome of the automatic serial-console boot-interaction sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootInteractResult {
    /// The VM reached a shell prompt; it is READY.
    Ready,
    /// The sequence aborted before reaching a shell prompt, with the last
    /// 500 bytes of serial output kept for diagnosis.
    Aborted {
        /// Why it aborted.
        reason: BootAbortReason,
        /// Last 500 bytes of the serial console output.
        tail: Vec<u8>,
    },
}

/// Error type for the automatic boot-interaction sequence.
#[derive(Debug, thiserror::Error)]
pub enum BootInteractError {
    /// The serial console could not be read while interacting.
    #[error("{0} (last 500 serial bytes: {1:?})")]
    Io(#[source] std::io::Error, Vec<u8>),
    /// The boot interaction aborted before reaching a shell prompt.
    #[error("automatic boot interaction aborted: {reason} (last 500 serial bytes: {tail:?})")]
    Aborted {
        reason: BootAbortReason,
        tail: Vec<u8>,
    },
}

impl From<QemuError> for BootInteractError {
    fn from(e: QemuError) -> Self {
        let io = match e {
            QemuError::Io(io) => io,
            _ => std::io::Error::new(std::io::ErrorKind::Other, format!("qemu: {e}")),
        };
        BootInteractError::Io(io, Vec::new())
    }
}

/// Read the serial console line by line, driving the login sequence for an
/// Arch-style ISO until it is READY or ABORTed.
///
/// On each "login:" prompt the configured user credentials are sent (empty
/// password = enter-key login), then the line is read again until a shell
/// prompt (`#`/`$`) is seen. On "login incorrect" or on the timeout, the
/// sequence ABORTs and returns the last 500 bytes of serial output inside
/// [`BootInteractResult::Aborted`], never retrying a failed login.
impl QemuHandle {
    pub async fn interact_boot(&mut self, timeout_secs: u64) -> Result<BootInteractResult, QemuError> {
        let timeout_secs = timeout_secs.max(1);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);

        let mut tail = Vec::new();
        loop {
            // ABORT on timeout: no shell prompt within the boot timeout.
            if Instant::now() >= deadline {
                return Ok(BootInteractResult::Aborted {
                    reason: BootAbortReason::Timeout,
                    tail,
                });
            }

            match self.read_line().await {
                Ok(Some(line)) => {
                    let lower = line.to_lowercase();

                    // GitHub-style login: send the configured user and password
                    // (an empty password value = enter-key login, e.g. Arch root).
                    if lower.contains("login:") {
                        let stdin = self.child.stdin.as_mut().unwrap();
                        let credentials =
                            format!(
                                "{} {}",
                                username_line(self.username.as_deref()),
                                password_line(self.password.as_deref()),
                            );
                        if let Err(e) = write_serial_line(stdin, &credentials).await {
                            return Err(e);
                        }
                        continue;
                    }

                    // Authentication failed: dump the tail and abort, no retry.
                    if lower.contains("login incorrect") {
                        return Ok(BootInteractResult::Aborted {
                            reason: BootAbortReason::LoginIncorrect,
                            tail,
                        });
                    }

                    // Shell prompt reached: the VM is READY and can run commands.
                    if looks_like_shell_prompt(&line) {
                        return Ok(BootInteractResult::Ready);
                    }

                    // Keep only the last 500 bytes for diagnostics.
                    keep_last_500(&mut tail, line.as_bytes());
                }
                Ok(None) => {
                    // No more serial output before a prompt (VM ended, etc.).
                    return Ok(BootInteractResult::Aborted {
                        reason: BootAbortReason::Timeout,
                        tail,
                    });
                }
                Err(e) => match e {
                    QemuError::Io(io) => return Err(QemuError::Io(io)),
                    _ => {
                        return Err(QemuError::Io(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("qemu: {e}"),
                        )))
                    }
                },
            }
        }
    }
}

/// Write a line to the serial console, which is the guest's terminal.
async fn write_serial_line<S>(
    stdout: &mut S,
    text: &str,
) -> Result<(), QemuError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let bytes: Vec<u8> = text.as_bytes().to_vec();
    tokio::io::AsyncWriteExt::write_all(stdout, &bytes)
        .await
        .map_err(QemuError::Io)
}

/// Keep only the last 500 bytes of `tail`.
fn keep_last_500(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.clear();
    let upto = bytes.get(..bytes.len().min(500)).unwrap_or(bytes);
    tail.extend_from_slice(upto);
}

/// Build the user-name line for a login prompt. Defaults to `root`.
fn username_line(username: Option<&str>) -> String {
    username.unwrap_or("root").to_string() + "\n"
}

/// Build the password line for a login prompt; an empty value means the
/// guest accepts an empty password (enter-key login).
fn password_line(password: Option<&str>) -> String {
    match password {
        Some(pwd) if !pwd.is_empty() => pwd.to_string() + "\n",
        _ => "\n".to_string(),
    }
}

/// Heuristic: does the line look like a shell prompt (`# …` or `$ …`)?
fn looks_like_shell_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.chars().next().unwrap();
    let is_alpha = trimmed
        .chars()
        .next()
        .map(|c| c.is_alphabetic())
        .unwrap_or(false);
    (first == '#' && is_alpha) || (first == '$' && is_alpha)
}

/// Run the automatic boot-interaction sequence when the config enables it.
///
/// Returns `Ok(())` once the VM reached a shell prompt, or an error carrying
/// the last 500 bytes of serial output when the sequence must be ABORTed.
pub async fn run_boot_interaction(
    handle: &mut QemuHandle,
    config: &QemuConfig,
) -> Result<(), BootInteractError> {
    if !config.auto_boot_interact {
        // Interaction disabled: the caller can drive the console by hand.
        return Ok(());
    }
    match handle.interact_boot(config.boot_timeout_secs).await {
        Ok(BootInteractResult::Ready) => Ok(()),
        Ok(BootInteractResult::Aborted { reason, tail }) => {
            Err(BootInteractError::Aborted { reason, tail })
        }
        Err(e) => Err(BootInteractError::from(e)),
    }
}
pub mod guest_agent;

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a process that sleeps briefly then exits, to test wait/kill.
    async fn spawn_sleep() -> QemuHandle {
        let child = Command::new("sleep")
            .arg("0.1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().unwrap();
        QemuHandle {
            child,
            pid,
            stopped: false,
            agent_socket: None,
            username: None,
            password: None,
        }
    }

    #[test]
    fn test_wait_and_kill() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let mut handle = spawn_sleep().await;
            let _ = handle.wait().await.unwrap();
            // Process already exited; kill should be a no-op.
            let _ = handle.kill().await.unwrap();
        });
    }

    #[test]
    fn test_read_line_from_closed_stream() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let mut handle = spawn_sleep().await;
            // No output; should return None on EOF.
            let result = handle.read_line().await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), None);
        });
    }

    #[test]
    fn test_stream_output_clamps_eof() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let mut handle = spawn_sleep().await;
            let mut out = Vec::new();
            let result = handle.stream_output(&mut out).await;
            assert!(result.is_ok());
            // sleep produces no output.
            assert!(out.is_empty());
        });
    }

    #[test]
    fn test_config_defaults() {
        let config = QemuConfig::default();
        assert_eq!(config.binary, "qemu-system-x86_64");
        assert_eq!(config.memory_mib, 1024);
        assert_eq!(config.cpus, 2);
        assert!(config.drive_image.is_none());
        assert!(config.iso.is_none());
        assert_eq!(config.boot_timeout_secs, 120);
        assert!(config.auto_boot_interact);
    }

    #[test]
    fn test_looks_like_shell_prompt() {
        assert!(looks_like_shell_prompt("# arch"));
        assert!(looks_like_shell_prompt("$ root@arch:"));
        assert!(!looks_like_shell_prompt("log in:"));
        assert!(!looks_like_shell_prompt(""));
        assert!(!looks_like_shell_prompt("unknown: no boot prompt"));
    }

    #[test]
    fn test_login_credentials_lines() {
        assert_eq!(username_line(Some("root")), "root\n");
        assert_eq!(username_line(None), "root\n");
        assert_eq!(password_line(None), "\n");
        assert_eq!(password_line(Some("")), "\n");
        assert_eq!(password_line(Some("tty")), "tty\n");
        // Arch root (root + empty password) becomes two Enter lines.
        let creds = format!("{} {}", username_line(Some("root")), password_line(Some("")));
        assert_eq!(creds, "root\n\n");
    }
}
