//! One-shot VM test runner: the high-level flow behind
//! *"test my bash script in Linux Alpine"*.
//!
//! Given a [`RunRequest`] (distro + script), the runner:
//!
//! 1. resolves the distro **profile** (ISO URL, serial-console credentials,
//!    guest shell, QEMU binary for the requested architecture);
//! 2. **fetches the ISO** if it is not already cached
//!    ([`ImageManager::ensure_iso`]);
//! 3. builds a **recipe** describing the exact approach taken and stores it
//!    in the run directory, so any later reader (agent or human) can see how
//!    the VM was assembled;
//! 4. **boots** the VM as a live ISO with automatic serial-console login
//!    ([`QemuHandle::spawn`] with `auto_boot_interact`);
//! 5. **injects the script** through the serial console via a quoted
//!    heredoc, runs it under the profile's shell, and waits for an
//!    `EXIT` sentinel carrying the script's exit code;
//! 6. **inspects** the captured output for failure patterns
//!    ([`classify_failures`]);
//! 7. **tears the VM down** (poweroff, then SIGKILL fallback) and
//! 8. **writes the report** — `report.md` (human/agent readable) and
//!    `report.json` (machine readable) — into the run directory.
//!
//! Failure of the *script* or the *boot* is captured **in the report**, not
//! returned as an error: the caller (an agent) wants to read what happened.
//! Only preconditions that make a run impossible (unknown distro, download
//! failure) are returned as [`RunnerError`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::info;

use mcp_qemu_core::{QemuError, QemuHandle};
use mcp_qemu_image_manager::ImageManager;
use mcp_qemu_profiles::Arch;

/// Directories the runner reads from and writes to.
#[derive(Debug, Clone)]
pub struct RunPaths {
    /// Where images/ISOs are cached (`images/isos/…`).
    pub images_dir: PathBuf,
    /// Where run reports are written (`runs/<run_id>/…`).
    pub runs_dir: PathBuf,
}

/// A one-shot script run request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunRequest {
    /// Distro name from the profile registry (e.g. `alpine`).
    pub distro: String,
    /// Optional ISO variant (e.g. `virt`, `standard`); default per registry.
    #[serde(default)]
    pub variant: Option<String>,
    /// Guest architecture; default x86_64.
    #[serde(default)]
    pub arch: Option<Arch>,
    /// The script (shell text) to run inside the VM.
    pub script: String,
    /// Guest memory in MiB (default 1024).
    #[serde(default)]
    pub memory_mib: Option<u64>,
    /// Guest CPUs (default 2).
    #[serde(default)]
    pub cpus: Option<u64>,
    /// Overall script timeout in seconds (default 300).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Boot-to-shell timeout in seconds (default 240).
    #[serde(default)]
    pub boot_timeout_secs: Option<u64>,
}

/// How a run ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunStatus {
    /// The script ran and exited 0.
    Success,
    /// The script ran and exited non-zero.
    ScriptFailure {
        /// The script's exit code.
        exit_code: i32,
    },
    /// The VM never reached a shell prompt.
    BootFailure {
        /// Why boot was aborted (reason + last serial bytes).
        detail: String,
    },
    /// The script never finished within the timeout.
    ScriptTimeout,
    /// Boot never reached a shell within the boot timeout.
    BootTimeout,
}

impl RunStatus {
    /// True when the script completed successfully.
    pub fn is_success(&self) -> bool {
        matches!(self, RunStatus::Success)
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Success => write!(f, "success"),
            RunStatus::ScriptFailure { exit_code } => {
                write!(f, "script failed (exit {exit_code})")
            }
            RunStatus::BootFailure { .. } => write!(f, "boot failure"),
            RunStatus::ScriptTimeout => write!(f, "script timeout"),
            RunStatus::BootTimeout => write!(f, "boot timeout"),
        }
    }
}

/// The full result of a run, as written to `report.json` and returned to
/// MCP/CLI callers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Run identifier (timestamp + distro + variant + arch).
    pub run_id: String,
    pub distro: String,
    pub variant: String,
    pub version: String,
    pub arch: String,
    /// How the run ended.
    pub status: RunStatus,
    /// Script exit code (None when the script never completed).
    pub exit_code: Option<i32>,
    /// Failure-pattern lines found in the output (see [`classify_failures`]).
    pub errors: Vec<String>,
    /// Last `OUTPUT_TAIL_LINES` lines of the console output.
    pub output_tail: String,
    /// Wall-clock duration of the whole run (boot + script).
    pub duration_secs: u64,
    /// Run start time (unix seconds).
    pub started_at_unix: u64,
    /// Directory holding all artifacts of this run.
    pub run_dir: PathBuf,
    /// Full serial console transcript (`serial.log`).
    pub log_path: PathBuf,
    /// The recipe that describes how this VM was assembled.
    pub recipe_path: PathBuf,
    /// This report as markdown (`report.md`).
    pub report_path: PathBuf,
}

/// Errors that make a run impossible before the VM is even useful.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// No profile in the registry for this distro/variant/arch.
    #[error("no profile for distro \"{distro}\" (see `distro_list`)")]
    ProfileNotFound {
        /// The requested distro name.
        distro: String,
    },
    /// Image/ISO provisioning failed.
    #[error("image error: {0}")]
    Image(#[from] mcp_qemu_image_manager::ImageError),
    /// Recipe validation/apply failed.
    #[error("recipe error: {0}")]
    Recipe(#[from] mcp_qemu_recipe::RecipeError),
    /// Local I/O failed while staging the run.
    #[error("io error: {0}")]
    Io(#[source] std::io::Error),
    /// The QEMU process failed to spawn or interact.
    #[error("qemu error: {0}")]
    Qemu(#[from] QemuError),
}

/// Result alias using [`RunnerError`].
pub type Result<T> = std::result::Result<T, RunnerError>;

/// Maximum lines kept in `output_tail`.
const OUTPUT_TAIL_LINES: usize = 200;
/// Script-injection chunk size (tty input buffers are small).
const SERIAL_CHUNK_BYTES: usize = 1024;
/// Pause between serial chunks so the guest tty can drain.
const SERIAL_CHUNK_PAUSE_MS: u64 = 40;

/// Run a script inside an ephemeral VM and produce a report.
///
/// This is the whole "test my script in distro X" flow in one call. See the
/// crate docs for the step list.
pub async fn run(request: RunRequest, paths: RunPaths) -> Result<RunReport> {
    let started_at = SystemTime::now();
    let start_unix = started_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // 1. Resolve the profile.
    let arch = request.arch.unwrap_or(Arch::X86_64);
    let profile = mcp_qemu_profiles::lookup(
        &request.distro,
        request.variant.as_deref(),
        arch,
    )
    .ok_or_else(|| RunnerError::ProfileNotFound {
        distro: request.distro.clone(),
    })?;

    // 2. Fetch the ISO if required (no-op when cached).
    let images = ImageManager::new(&paths.images_dir);
    let iso_path = images.ensure_iso(profile.iso_url).await?;

    // 3. Run directory + recorded artifacts.
    let run_id = format!(
        "{}-{}-{}-{}",
        start_unix,
        profile.distro,
        profile.variant,
        arch.as_str()
    );
    let run_dir = paths.runs_dir.join(&run_id);
    tokio::fs::create_dir_all(&run_dir).await.map_err(RunnerError::Io)?;
    let script_path = run_dir.join("script.sh");
    tokio::fs::write(&script_path, &request.script)
        .await
        .map_err(RunnerError::Io)?;

    let memory_mib = request.memory_mib.unwrap_or(1024);
    let cpus = request.cpus.unwrap_or(2);
    let timeout_secs = request.timeout_secs.unwrap_or(300);
    // Live-ISO boots under TCG (no KVM) are slow — 10 minutes of headroom.
    let boot_timeout_secs = request.boot_timeout_secs.unwrap_or(600);

    // The recipe records the exact approach taken for this run.
    let recipe = recipe_record(profile, arch, &iso_path, memory_mib, cpus, boot_timeout_secs);
    let recipe_path = run_dir.join("recipe.yaml");
    tokio::fs::write(
        &recipe_path,
        serde_yaml::to_string(&recipe)
            .map_err(mcp_qemu_recipe::RecipeError::from)
            .map_err(RunnerError::from)?,
    )
    .await
    .map_err(RunnerError::Io)?;

    // 4. Boot via the recipe path so MCP and CLI share one config builder.
    let mut handle = match QemuHandle::spawn(recipe.clone().apply()?).await {
        Ok(handle) => handle,
        Err(QemuError::BootInteract(err)) => {
            // Boot failed: capture and report, no error return.
            return finish_run(FinishedRun {
                report_fields: (&request, profile, arch, run_id, run_dir, started_at, start_unix),
                status: RunStatus::BootFailure {
                    detail: err.to_string(),
                },
                exit_code: None,
                output: Vec::new(),
            })
            .await;
        }
        Err(e) => return Err(e.into()),
    };
    info!("VM ready (pid {}); injecting script", handle.pid());

    // 5. Inject + run the script under the profile's shell.
    let sentinel = format!("MCPQ_EXIT_{}", start_unix);
    let bootstrap = build_bootstrap(&request.script, profile.shell, &sentinel);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut output: Vec<String> = Vec::new();
    let (status, exit_code) = match drive_script(&mut handle, &bootstrap, &sentinel, deadline, &mut output).await {
        ScriptOutcome::Exit(code) if code == 0 => (RunStatus::Success, Some(code)),
        ScriptOutcome::Exit(code) => (
            RunStatus::ScriptFailure { exit_code: code },
            Some(code),
        ),
        ScriptOutcome::TimedOut => (RunStatus::ScriptTimeout, None),
        ScriptOutcome::Serial(e) => (
            RunStatus::BootFailure {
                detail: format!("serial console failed while running script: {e}"),
            },
            None,
        ),
    };

    // 6. Tear down (poweroff first for a clean guest shutdown, then SIGKILL).
    let _ = handle.send_line("poweroff").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let _ = handle.kill().await;

    // 7. Report.
    finish_run(FinishedRun {
        report_fields: (&request, profile, arch, run_id, run_dir, started_at, start_unix),
        status,
        exit_code,
        output,
    })
    .await
}

/// Everything needed to write the report once the VM interaction is over.
struct FinishedRun<'a> {
    report_fields: (
        &'a RunRequest,
        &'static mcp_qemu_profiles::DistroProfile,
        Arch,
        String,
        PathBuf,
        SystemTime,
        u64,
    ),
    status: RunStatus,
    exit_code: Option<i32>,
    output: Vec<String>,
}

/// Write serial.log / report.md / report.json and assemble the [`RunReport`].
async fn finish_run(run: FinishedRun<'_>) -> Result<RunReport> {
    let FinishedRun {
        report_fields:
            (request, profile, arch, run_id, run_dir, started_at, start_unix),
        status,
        exit_code,
        output,
    } = run;
    let _ = request; // kept for future per-request report fields

    let log_path = run_dir.join("serial.log");
    tokio::fs::write(&log_path, output.join("\n"))
        .await
        .map_err(RunnerError::Io)?;

    let tail_start = output.len().saturating_sub(OUTPUT_TAIL_LINES);
    let output_tail = output[tail_start..].join("\n");
    let errors = classify_failures(&output);
    let duration_secs = started_at.elapsed().unwrap_or_default().as_secs();

    let report = RunReport {
        run_id,
        distro: profile.distro.to_string(),
        variant: profile.variant.to_string(),
        version: profile.version.to_string(),
        arch: arch.as_str().to_string(),
        status: status.clone(),
        exit_code,
        errors: errors.clone(),
        output_tail: output_tail.clone(),
        duration_secs,
        started_at_unix: start_unix,
        run_dir: run_dir.clone(),
        log_path: log_path.clone(),
        recipe_path: run_dir.join("recipe.yaml"),
        report_path: run_dir.join("report.md"),
    };

    let markdown = render_markdown(&report, &output_tail);
    tokio::fs::write(&report.report_path, &markdown)
        .await
        .map_err(RunnerError::Io)?;
    tokio::fs::write(
        run_dir.join("report.json"),
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .await
    .map_err(RunnerError::Io)?;
    info!("run {} finished: {}", report.run_id, status);

    Ok(report)
}

/// Build the run-record recipe describing exactly how this VM was assembled.
fn recipe_record(
    profile: &mcp_qemu_profiles::DistroProfile,
    arch: Arch,
    iso_path: &Path,
    memory_mib: u64,
    cpus: u64,
    boot_timeout_secs: u64,
) -> mcp_qemu_recipe::Recipe {
    mcp_qemu_recipe::Recipe {
        image: None,
        iso: Some(iso_path.to_path_buf()),
        arch: Some(arch.as_str().to_string()),
        memory_mib,
        cpus,
        command: profile.shell.to_string(),
        args: Vec::new(),
        env: Vec::new(),
        qemu_args: Vec::new(),
        username: Some(profile.login.username.to_string()),
        password: Some(profile.login.password.to_string()),
        boot_prompt: profile.boot_prompt.map(str::to_string),
        boot_response: profile.boot_response.map(str::to_string),
        boot_timeout_secs: Some(boot_timeout_secs),
    }
}

/// Build the serial-console bootstrap that writes the script into the guest
/// and runs it, echoing the exit code through a `sentinel=…` line.
///
/// The heredoc is single-quoted so the guest shell performs no expansion of
/// the script text. A leading `stty -echo` keeps the guest from echoing the
/// injected bytes back into the log (and out of failure classification).
fn build_bootstrap(script: &str, shell: &str, sentinel: &str) -> String {
    format!(
        "stty -echo 2>/dev/null; cat > /tmp/mcpq_script.sh <<'MCPQ_SCRIPT_EOF'\n\
         {script}\nMCPQ_SCRIPT_EOF\n\
         {shell} /tmp/mcpq_script.sh; echo \"{sentinel}=$?\"\n"
    )
}

/// Outcome of the script-driving phase.
enum ScriptOutcome {
    /// Script finished with the given exit code (sentinel seen).
    Exit(i32),
    /// Deadline hit before the sentinel appeared.
    TimedOut,
    /// Serial console I/O failed mid-run.
    Serial(QemuError),
}

/// Send the bootstrap through the serial console (chunked for tty buffers),
/// then read lines until the sentinel or the deadline.
async fn drive_script(
    handle: &mut QemuHandle,
    bootstrap: &str,
    sentinel: &str,
    deadline: Instant,
    output: &mut Vec<String>,
) -> ScriptOutcome {
    // Chunk the write: guest tty buffers are small (~4 KiB) and a large
    // single write can be truncated.
    for chunk in chunks_of(bootstrap, SERIAL_CHUNK_BYTES) {
        if let Err(e) = handle.send_text(&chunk).await {
            return ScriptOutcome::Serial(e);
        }
        tokio::time::sleep(Duration::from_millis(SERIAL_CHUNK_PAUSE_MS)).await;
    }

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ScriptOutcome::TimedOut;
        }
        let line = match tokio::time::timeout(remaining, handle.read_line()).await {
            Err(_) => return ScriptOutcome::TimedOut,
            Ok(Err(e)) => return ScriptOutcome::Serial(e),
            Ok(Ok(None)) => return ScriptOutcome::TimedOut, // console closed
            Ok(Ok(Some(line))) => line,
        };

        // The sentinel output line is `<sentinel>=<code>`. The echoed command
        // line contains `"$?"` (not digits) so it can never match here.
        let prefix = format!("{sentinel}=");
        if let Some(rest) = line.trim().strip_prefix(&prefix) {
            if let Ok(code) = rest.trim().parse::<i32>() {
                return ScriptOutcome::Exit(code);
            }
        }

        output.push(line);
    }
}

/// Split `s` into chunks of at most `size` chars.
fn chunks_of(s: &str, size: usize) -> Vec<String> {
    s.chars()
        .collect::<Vec<_>>()
        .chunks(size)
        .map(|c| c.iter().collect())
        .collect()
}

/// Scan the console output for failure patterns (case-insensitive).
///
/// Matches common failure shapes: missing commands/files, permission
/// errors, crashes, and `error:`/`fatal`/`fail` prefixes. Returns deduped,
/// trimmed lines (capped at 50).
pub fn classify_failures(output: &[String]) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    for line in output {
        let trimmed = line.trim();
        if trimmed.is_empty() || errors.contains(&trimmed.to_string()) {
            continue;
        }
        let lower = trimmed.to_lowercase();
        let hit = lower.contains("not found")
            || lower.contains("no such file")
            || lower.contains("permission denied")
            || lower.contains("segmentation fault")
            || lower.contains("panic")
            || lower.contains("error:")
            || lower.contains("fatal")
            || lower.starts_with("fail");
        if hit {
            errors.push(trimmed.to_string());
        }
    }
    errors.truncate(50);
    errors
}

/// Render the human/agent-readable `report.md`.
fn render_markdown(report: &RunReport, output_tail: &str) -> String {
    let mut md = String::new();
    md.push_str(&format!("# VM run report — {}\n\n", report.run_id));
    md.push_str(&format!("**Status:** {}\n\n", report.status));
    if let RunStatus::BootFailure { detail } = &report.status {
        md.push_str(&format!("> Boot failure detail: {detail}\n\n"));
    }
    md.push_str("## Machine\n\n");
    md.push_str(&format!(
        "| distro | variant | version | arch | duration |\n|---|---|---|---|---|\n| {} | {} | {} | {} | {}s |\n\n",
        report.distro, report.variant, report.version, report.arch, report.duration_secs
    ));
    md.push_str(&format!(
        "Exit code: {}\n\n",
        report
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "n/a (script did not complete)".to_string())
    ));
    if report.errors.is_empty() {
        md.push_str("## Failures\n\nNo failure patterns detected in the output.\n\n");
    } else {
        md.push_str("## Failures (detected patterns)\n\n");
        for e in &report.errors {
            md.push_str(&format!("- `{e}`\n"));
        }
        md.push('\n');
    }
    md.push_str("## Console output (tail)\n\n```text\n");
    md.push_str(output_tail);
    md.push_str("\n```\n\n");
    md.push_str("## Artifacts\n\n");
    md.push_str(&format!(
        "- recipe (approach used): `{}`\n- full serial log: `{}`\n- machine-readable report: `{}`\n",
        report
            .recipe_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        log_file_name(&report.log_path),
        "report.json",
    ));
    md
}

/// File name of a log path (helper for markdown rendering).
fn log_file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// List finished runs (one summary per `runs/<run_id>/report.json`).
pub async fn report_list(runs_dir: &Path) -> std::io::Result<Vec<RunReport>> {
    let mut reports = Vec::new();
    if !runs_dir.exists() {
        return Ok(reports);
    }
    let mut entries = tokio::fs::read_dir(runs_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let path = entry.path().join("report.json");
        if let Ok(text) = tokio::fs::read_to_string(&path).await {
            if let Ok(report) = serde_json::from_str::<RunReport>(&text) {
                reports.push(report);
            }
        }
    }
    reports.sort_by(|a, b| b.run_id.cmp(&a.run_id));
    Ok(reports)
}

/// Read one run's markdown report. `run_id` must be a plain directory name.
pub async fn report_get(runs_dir: &Path, run_id: &str) -> std::io::Result<String> {
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid run_id",
        ));
    }
    tokio::fs::read_to_string(runs_dir.join(run_id).join("report.md")).await
}
