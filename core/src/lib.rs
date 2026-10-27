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
use tokio::sync::mpsc;

/// Error type for all QEMU lifecycle operations.
#[derive(Debug, thiserror::Error)]
pub enum QemuError {
    /// The QEMU binary was not found on PATH.
    #[error("QEMU binary not found on PATH")]
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
    /// Boot-loader prompt marker to watch for on the serial console
    /// (e.g. ISOLINUX `boot:`). When seen (and not yet answered),
    /// `boot_response` is sent as one line.
    pub boot_prompt: Option<String>,
    /// Line to send at the boot-loader prompt (kernel + initrd + serial
    /// console kernel argument).
    pub boot_response: Option<String>,
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
            boot_prompt: None,
            boot_response: None,
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
    /// Boot-loader prompt marker to answer (see [`QemuConfig::boot_prompt`]).
    boot_prompt: Option<String>,
    /// Line to send when the boot-loader prompt appears.
    boot_response: Option<String>,
    /// Serial console (child stdout) reader channel. The bytes are pumped
    /// here by a dedicated task so that timed-out reads (boot interaction,
    /// script deadlines) never lose data: a cancelled `recv` leaves the
    /// bytes buffered in the channel.
    stdout_rx: Option<mpsc::Receiver<std::io::Result<u8>>>,
    /// Child stderr reader channel (QEMU diagnostics).
    stderr_rx: Option<mpsc::Receiver<std::io::Result<u8>>>,
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
            .arg("none");

        // Drive image.
        if let Some(img) = &config.drive_image {
            cmd.arg("-drive")
                .arg(format!("file={},format=qcow2", img));
        }

        // ISO attachment. On x86 the classic `-cdrom` shorthand works; on
        // aarch64 `-M virt` has no IDE controller, so the CD-ROM is hung
        // off a virtio-scsi device instead.
        if let Some(iso) = &config.iso {
            if config.binary.contains("aarch64") {
                cmd.arg("-device").arg("virtio-scsi-pci,id=mcpq-scsi")
                    .arg("-drive")
                    .arg(format!("if=none,id=mcpq-cd,file={},media=cdrom,format=raw", iso))
                    .arg("-device").arg("scsi-cd,drive=mcpq-cd");
            } else {
                cmd.arg("-cdrom").arg(iso);
            }
        }

        // Boot order: when an ISO is the boot medium, boot from it first —
        // otherwise SeaBIOS stalls on "Booting from Hard Disk..." forever.
        if config.iso.is_some() {
            cmd.arg("-boot").arg("d");
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

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .map_err(QemuError::Spawn)?;

        let pid = child.id().ok_or(QemuError::QemuNotFound)?;

        let agent_socket = config.agent_socket;

        // Pump child stdout/stderr into buffered channels via dedicated
        // reader tasks. Tokio's child stdio reads are NOT cancellation-safe
        // (they run through a blocking thread), so any `timeout(read)`
        // cancellation would silently swallow bytes. With the pump, a
        // cancelled `recv()` simply leaves bytes buffered in the channel.
        let stdout_rx = pump_stream(child.stdout.take());
        let stderr_rx = pump_stream(child.stderr.take());

        // Run the automatic serial-console boot interaction when configured.
        let mut handle = Self {
            child,
            pid,
            stopped: false,
            agent_socket,
            username: config.username,
            password: config.password,
            boot_prompt: config.boot_prompt,
            boot_response: config.boot_response,
            stdout_rx,
            stderr_rx,
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
        let mut line = Vec::new();
        let rx = match self.stdout_rx.as_mut() {
            Some(rx) => rx,
            None => return Ok(None),
        };
        loop {
            match rx.recv().await {
                None => {
                    // Stream closed; return any buffered line.
                    if line.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(String::from_utf8_lossy(&line).to_string()));
                }
                Some(Err(e)) => return Err(QemuError::Io(e)),
                Some(Ok(b)) => {
                    line.push(b);
                    if b == b'\n' {
                        // Strip trailing newline (and CR).
                        line.pop();
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        return Ok(Some(String::from_utf8_lossy(&line).to_string()));
                    }
                }
            }
        }
    }

    /// Write raw text to the guest's serial console (its terminal stdin),
    /// with no trailing newline. Use [`QemuHandle::send_line`] for single
    /// lines.
    pub async fn send_text(&mut self, text: &str) -> Result<(), QemuError> {
        let stdin = self.child.stdin.as_mut().unwrap();
        write_serial_line(stdin, text).await
    }

    /// Write one line (text + `\n`) to the guest's serial console, as if the
    /// user had typed it and pressed Enter.
    pub async fn send_line(&mut self, text: &str) -> Result<(), QemuError> {
        let stdin = self.child.stdin.as_mut().unwrap();
        write_serial_line(stdin, &format!("{text}\n")).await
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
        // Drain each channel into a Vec until EOF, then forward to the writer.
        let stdout_data = drain_channel(&mut self.stdout_rx).await;
        let stderr_data = drain_channel(&mut self.stderr_rx).await;

        let _ = writer.write(&stdout_data).await;
        let _ = writer.write(&stderr_data).await;

        Ok(())
    }
}

/// Drain one reader channel into a Vec until EOF.
async fn drain_channel(rx: &mut Option<mpsc::Receiver<std::io::Result<u8>>>) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(rx) = rx else { return out };
    while let Some(item) = rx.recv().await {
        match item {
            Ok(b) => out.push(b),
            Err(_) => break,
        }
    }
    out
}

/// Spawn a task that pumps `stream` bytes into a buffered channel, so
/// timed-out/cancelled reads never lose data (tokio child stdio reads are
/// not cancellation-safe).
fn pump_stream(
    stream: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
) -> Option<mpsc::Receiver<std::io::Result<u8>>> {
    let stream = stream?;
    let (tx, rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        let mut stream = stream;
        let mut buf = [0u8; 512];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    for b in &buf[..n] {
                        if tx.send(Ok(*b)).await.is_err() {
                            return; // receiver dropped
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });
    Some(rx)
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
    #[error("automatic boot interaction aborted: {reason} (last 500 serial bytes: {tail_text})", tail_text = String::from_utf8_lossy(&tail))]
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
    /// Drive the serial console through the boot sequence until a shell
    /// prompt is reached.
    ///
    /// Reads the console **byte by byte** (with a 50 ms idle tick) so that
    /// prompt lines that never end with a newline (e.g. `localhost:~# `)
    /// are still detected. On each `login:` prompt the configured
    /// credentials are sent (empty password = enter-key login). On
    /// "login incorrect" or the boot timeout the sequence ABORTs with the
    /// last 500 bytes of serial output, never retrying a failed login.
    pub async fn interact_boot(&mut self, timeout_secs: u64) -> Result<BootInteractResult, QemuError> {
        let timeout_secs = timeout_secs.max(1);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let tick = Duration::from_millis(50);

        let mut tail = Vec::new();
        let mut line = Vec::new();
        // Login stage machine: 0 = awaiting `login:`, 1 = awaiting
        // `Password:` (or directly the shell for enter-key logins), 2 =
        // credentials fully sent.
        let mut login_stage = 0u8;
        // Answer the boot-loader prompt at most once.
        let mut boot_answered = false;

        loop {
            // ABORT on timeout: no shell prompt within the boot timeout.
            if Instant::now() >= deadline {
                return Ok(BootInteractResult::Aborted {
                    reason: BootAbortReason::Timeout,
                    tail,
                });
            }

            let read_result = {
                let rx = match self.stdout_rx.as_mut() {
                    Some(rx) => rx,
                    None => {
                        return Ok(BootInteractResult::Aborted {
                            reason: BootAbortReason::Timeout,
                            tail,
                        })
                    }
                };
                tokio::time::timeout(tick, rx.recv()).await
            };
            let b: u8 = match read_result {
                Err(_elapsed) => {
                    // Console idle. A newline-less prompt may be sitting in
                    // `line`: either a shell prompt (READY), a login prompt,
                    // or a boot-loader prompt that must be answered.
                    if is_partial_shell_prompt(&line) {
                        return Ok(BootInteractResult::Ready);
                    }
                    if !boot_answered {
                        if let (Some(marker), Some(response)) =
                            (self.boot_prompt.as_deref(), self.boot_response.as_deref())
                        {
                            if contains_substring(&line, marker.as_bytes()) {
                                let stdin = self.child.stdin.as_mut().unwrap();
                                write_serial_line(stdin, &format!("{response}\n")).await?;
                                boot_answered = true;
                                line.clear();
                                keep_last_500(&mut tail, marker.as_bytes());
                            }
                        }
                    }
                    // Getty prints `… login: ` WITHOUT a trailing newline,
                    // so this is usually caught here (idle), not in the
                    // complete-line path below.
                    if login_stage == 0 && contains_substring(&line, b"login:") {
                        let stdin = self.child.stdin.as_mut().unwrap();
                        let credentials = username_line(self.username.as_deref());
                        write_serial_line(stdin, &credentials).await?;
                        login_stage = 1;
                        line.clear();
                        keep_last_500(&mut tail, b"login:");
                        continue;
                    }
                    if login_stage == 1 && contains_substring(&line, b"Password:") {
                        let stdin = self.child.stdin.as_mut().unwrap();
                        let credentials = password_line(self.password.as_deref());
                        write_serial_line(stdin, &credentials).await?;
                        login_stage = 2;
                        line.clear();
                        keep_last_500(&mut tail, b"Password:");
                        continue;
                    }
                    continue;
                }
                Ok(None) => {
                    // EOF: QEMU closed stdout (it probably died). Drain its
                    // stderr so the failure carries an actual diagnostic.
                    if let Some(stderr_rx) = self.stderr_rx.as_mut() {
                        let mut errbuf = Vec::new();
                        loop {
                            match tokio::time::timeout(
                                Duration::from_millis(200),
                                stderr_rx.recv(),
                            )
                            .await
                            {
                                Err(_) | Ok(None) | Ok(Some(Err(_))) => break,
                                Ok(Some(Ok(b))) => errbuf.push(b),
                            }
                            if errbuf.len() > 4096 {
                                break;
                            }
                        }
                        if !errbuf.is_empty() {
                            keep_last_500(&mut tail, &errbuf);
                        }
                    }
                    return Ok(BootInteractResult::Aborted {
                        reason: BootAbortReason::Timeout,
                        tail,
                    });
                }
                Ok(Some(Ok(b))) => b,
                Ok(Some(Err(e))) => return Err(QemuError::Io(e)),
            };

            line.push(b);

            if b != b'\n' {
                // Partial line: prompts do not end with a newline, so check
                // whether one has already appeared.
                if is_partial_shell_prompt(&line) {
                    return Ok(BootInteractResult::Ready);
                }
                continue;
            }

            // Complete line: strip the trailing newline (and CR).
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).to_string();
            line.clear();

            let lower = text.to_lowercase();

            // Boot-loader prompt (e.g. ISOLINUX `boot:`): send the profile's
            // boot line so the kernel console goes to the serial port.
            if !boot_answered {
                if let (Some(marker), Some(response)) =
                    (self.boot_prompt.as_deref(), self.boot_response.as_deref())
                {
                    if lower.contains(marker) {
                        let stdin = self.child.stdin.as_mut().unwrap();
                        write_serial_line(stdin, &format!("{response}\n")).await?;
                        boot_answered = true;
                        keep_last_500(&mut tail, text.as_bytes());
                        continue;
                    }
                }
            }

            // Login prompt (complete-line variant, for gettys that do end
            // the prompt with a newline): send username, then wait for
            // `Password:` — or, for enter-key logins, straight for the
            // shell.
            if login_stage == 0 && lower.contains("login:") {
                let stdin = self.child.stdin.as_mut().unwrap();
                let credentials = username_line(self.username.as_deref());
                write_serial_line(stdin, &credentials).await?;
                login_stage = 1;
                keep_last_500(&mut tail, text.as_bytes());
                continue;
            }

            // Password prompt (complete-line variant).
            if login_stage == 1 && lower.contains("password:") {
                let stdin = self.child.stdin.as_mut().unwrap();
                let credentials = password_line(self.password.as_deref());
                write_serial_line(stdin, &credentials).await?;
                login_stage = 2;
                keep_last_500(&mut tail, text.as_bytes());
                continue;
            }

            // Authentication failed: dump the tail and abort, no retry.
            if login_stage >= 1 && lower.contains("login incorrect") {
                return Ok(BootInteractResult::Aborted {
                    reason: BootAbortReason::LoginIncorrect,
                    tail,
                });
            }

            // Shell prompt reached: the VM is READY and can run commands.
            if looks_like_shell_prompt(&text) {
                return Ok(BootInteractResult::Ready);
            }

            // Keep only the last 500 bytes for diagnostics.
            keep_last_500(&mut tail, text.as_bytes());
        }
    }
}

/// True when the console is idle and the partial (newline-less) line ends
/// in a shell-prompt shape (`…# ` / `…$ `).
///
/// Requiring the trailing space avoids false positives on banner lines
/// like `### Welcome to Alpine Linux ###` (which end in `#`, not `# `).
fn is_partial_shell_prompt(line: &[u8]) -> bool {
    line.ends_with(b"# ") || line.ends_with(b"$ ")
}

/// True when `haystack` contains `needle` (byte-level substring).
fn contains_substring(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w == needle)
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

/// Append `bytes` to the diagnostic tail, keeping only the last 500 bytes.
///
/// Appending (then truncating) — rather than replacing — is essential:
/// empty/blank lines must not wipe out the useful history before them.
fn keep_last_500(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    if tail.len() > 500 {
        let drop = tail.len() - 500;
        tail.drain(..drop);
    }
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

/// Heuristic: does the line look like a shell prompt?
///
/// Matches two families of prompt lines:
/// - a *leading* prompt character: `#` or `$` alone or followed by
///   whitespace/text (e.g. `# arch`, `$ root@arch:`), but not a shebang
///   (`#!/bin/sh`, where `!` follows the `#`);
/// - a *trailing* prompt character, which is how most real guests print
///   prompts (e.g. `localhost:~#`, `[root@archiso ~]#`, `sh-5.1$`).
fn looks_like_shell_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut chars = trimmed.chars();
    let first = chars.next().unwrap();
    match first {
        '#' | '$' => match chars.next() {
            None => true,
            Some(c) => c.is_whitespace() || c.is_alphanumeric(),
        },
        _ => trimmed.ends_with('#') || trimmed.ends_with('$'),
    }
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
            boot_prompt: None,
            boot_response: None,
            stdout_rx: None,
            stderr_rx: None,
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
        // Real-world trailing prompts (Alpine, Arch, generic sh).
        assert!(looks_like_shell_prompt("localhost:~#"));
        assert!(looks_like_shell_prompt("[root@archiso ~]#"));
        assert!(looks_like_shell_prompt("sh-5.1$"));
        assert!(!looks_like_shell_prompt("log in:"));
        assert!(!looks_like_shell_prompt(""));
        assert!(!looks_like_shell_prompt("unknown: no boot prompt"));
        // Shebangs are not prompts.
        assert!(!looks_like_shell_prompt("#!/bin/sh"));
    }

    #[test]
    fn test_partial_shell_prompt() {
        // Newline-less prompts end with the prompt char + space.
        assert!(is_partial_shell_prompt(b"localhost:~# "));
        assert!(is_partial_shell_prompt(b"sh-5.1$ "));
        assert!(!is_partial_shell_prompt(b"### Welcome to Alpine Linux ###"));
        assert!(!is_partial_shell_prompt(b"localhost login: "));
        assert!(!is_partial_shell_prompt(b""));
    }

    #[test]
    fn test_login_credentials_lines() {
        assert_eq!(username_line(Some("root")), "root\n");
        assert_eq!(username_line(None), "root\n");
        assert_eq!(password_line(None), "\n");
        assert_eq!(password_line(Some("")), "\n");
        assert_eq!(password_line(Some("tty")), "tty\n");
        // Arch root (root + empty password) becomes two Enter lines.
        let creds = format!("{}{}", username_line(Some("root")), password_line(Some("")));
        assert_eq!(creds, "root\n\n");
    }
}
