# mcp-qemu-ephemeral

MCP server that hands AI agents disposable QEMU sandboxes — launch an ephemeral VM, execute inside it, destroy it, all while the base image stays untouched.

## Overview

`mcp-qemu-ephemeral` lets you spawn, control, and tear down isolated QEMU VMs, manage disk images (base images, qcow2 overlays, ISO attachments), define and apply recipes, and — the headline flow — **test a bash script inside a real Linux distro**:

> _"test my bash script in linux alpine"_ → fetch the distro ISO (sha256-verified) → boot the live ISO → auto-login on the serial console → run the script → capture output & failures → tear down → produce `report.md` / `report.json` the caller (agent or human) can read.

It is usable in two modes from the same codebase:

1. **CLI**: `mcp-qemu-ephemeral test --distro alpine script.sh`
2. **MCP server**: a stdio MCP server (kuri) exposing the same capabilities as tools.

| Crate           | Purpose                                                                                                                             |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `core`          | QEMU process lifecycle (`QemuHandle`, `QemuConfig`) + serial-console boot interaction (login state machine, boot-prompt answering). |
| `image-manager` | Disk image provisioning (base, qcow2, iso) + ISO download & sha256 verification.                                                    |
| `recipe`        | Recipe parsing (YAML/JSON) and VM config application; recipes double as _run records_.                                              |
| `profiles`      | Distro knowledge base: ISO URLs, serial-console credentials, guest shell, arch→QEMU binary mapping.                                 |
| `runner`        | One-shot orchestrator: fetch → boot → run → classify failures → report (`report.md`/`report.json`).                                 |
| `mcp-server`    | Kuri-based stdio MCP server exposing 15 tools.                                                                                      |
| `cli`           | CLI subcommands (`run`, `test`, `image`, `recipe`, `logs`).                                                                         |

## Installation

### For AI agents (standard flow)

Point any coding agent at this repository's setup document:

```text
Fetch and read <repo_url>/INSTALLATION.md, then follow the setup
instructions on this machine. Verify each step before moving to the next.
```

`INSTALLATION.md` is written to be followed end-to-end by an agent: prerequisites with per-distro package names, build, test, MCP-server registration snippets (Claude Desktop / opencode / Cursor / generic), a smoke test, an agent usage cookbook, and a common-failures table.

### Prerequisites

Prerequisites: Rust ≥ 1.75, QEMU (`qemu-system-x86_64`, `qemu-img`), `curl` or `wget`. Then:

```bash
cargo build --release
cargo test --workspace

# one-shot script test inside an ephemeral Alpine VM
./target/release/mcp-qemu-ephemeral test --distro alpine my-script.sh
```

Details, smoke test, and troubleshooting live in [`INSTALLATION.md`](INSTALLATION.md).

## Architecture

### Process management

QEMU is spawned as a child process via [`tokio::process`] with piped stdio so we can stream the VM's serial console output. The [`QemuHandle`] owns the child process and guarantees cleanup on `Drop` (no zombie QEMU processes).

- **spawn**: builds qemu args (`-nographic -serial stdio -monitor none`,
  `-drive`, `-cdrom` (+ `-boot d` when an ISO is the boot medium), `-m`,
  `-smp`, arch machine args for aarch64, and optionally `-enable-agent`
  for the Guest Agent socket).
- **Serial console plumbing**: stdout/stderr are pumped through `mpsc`
  channels by dedicated reader tasks — tokio child-stdio reads are _not_
  cancellation-safe, and the 50 ms tick reads used by boot interaction
  would otherwise silently swallow bytes.
- **interact_boot**: byte-driven boot loop that (a) answers a configured
  boot-loader prompt (`boot_prompt`/`boot_response`) if the profile
  declares one, (b) answers `login:` / `Password:` prompts with the
  profile's credentials (newline-less prompts included), and (c) detects
  the shell prompt (`…# ` / `…$ `, with or without trailing newline).
  Aborts with the last 500 serial bytes as diagnostics.
- **pause/resume**: SIGSTOP/SIGCONT via `libc::kill`.
- **stop**: SIGTERM with a grace period, then `kill()` (SIGKILL) as fallback.
- **Drop**: best-effort `kill()` to avoid leaking processes.
- **Guest Agent socket**: QEMU can export a Guest Agent unix socket via
  `-enable-agent unix:<path>`; the path is stored on `QemuHandle` so callers
  (CLI or MCP server) can connect to it.

### Distro profiles

The `profiles` crate is the knowledge layer: for each distro/variant/arch it carries the ISO mirror URL, serial-console `Login` (Alpine live ISOs: `root`, empty password), the guest shell, and free-form notes (e.g. "network is DOWN until `setup-interfaces -a -r`"). Adding a distro = adding one registry row.

### Images

- Base image: a qcow2/raw disk with an installed OS.
- qcow2 overlay: copy-on-write file at `<images_dir>/<name>.qcow2`, created via `qemu-img create -f qcow2 -b <base> <overlay>`.
- ISO: downloaded on demand into `<images_dir>/isos/` via `ensure_iso()`
  (curl→wget fallback, `.part` staging, sha256 verification against the
  mirror's companion file), and attached as `-cdrom` (+ `-boot d`).

### Recipes (also run records)

Recipes are YAML or JSON documents describing a VM's configuration:

```yaml
iso: images/isos/alpine-virt-3.22.7-x86_64.iso # ISO-only = live boot
arch: x86_64
memory_mib: 1024
cpus: 2
command: /bin/sh
username: root
password: "" # empty = enter-key login
boot_timeout_secs: 240
```

The recipe system parses, validates, and applies the recipe to spin up a VM via `QemuConfig`. `image:` (installed disk) and `iso:` (live boot) are both supported; every one-shot run writes its **effective recipe** into `runs/<run_id>/recipe.yaml` so any later reader can see exactly how the VM was assembled.

### Script runs & reports

The `runner` crate performs the whole flow and writes into `runs/<run_id>/`:

| File          | Content                                                                     |
| ------------- | --------------------------------------------------------------------------- |
| `recipe.yaml` | exactly how this VM was assembled (the "approach")                          |
| `script.sh`   | the script that was injected                                                |
| `serial.log`  | full serial-console transcript                                              |
| `report.md`   | human/agent-readable report                                                 |
| `report.json` | machine-readable report (status, exit code, detected failures, output tail) |

The script is injected over the serial console via a quoted heredoc, run under the profile's shell, and watched for an `EXIT` sentinel carrying the script's exit code. Failure patterns (`not found`, `permission denied`, `panic`, `error:`, …) are classified in the report. Script/boot failures appear **in the report**, not as errors.

### Guest Agent logs

To retrieve logs from _inside_ a running VM, the crate connects to the VM's Guest Agent unix socket and runs commands (e.g. `cat /var/log/syslog`) via the Guest Agent protocol. As a fallback, the serial console can be streamed.

## CLI usage

```bash
# One-shot: fetch ISO if needed, boot, run script, report, tear down
./target/release/mcp-qemu-ephemeral test --distro alpine [--variant virt] [--arch x86_64] script.sh

# Legacy: spawn with the default config and stream the console
./target/release/mcp-qemu-ephemeral run sandbox.sh
```

`test` prints the markdown report to stdout, writes the artifacts into
`runs/<run_id>/`, and exits non-zero when the script/boot fails.

## MCP server usage

The MCP server runs over stdio and exposes 15 tools:

| Tool                     | Description                                                         |
| ------------------------ | ------------------------------------------------------------------- |
| `qemu_start`             | Start a QEMU VM from a JSON config; returns the VM PID.             |
| `qemu_stop`              | Stop a running VM by PID.                                           |
| `qemu_pause`             | Pause a VM by PID (SIGSTOP).                                        |
| `qemu_resume`            | Resume a VM by PID (SIGCONT).                                       |
| `qemu_output`            | Stream stdout/stderr from a VM by PID until exit.                   |
| `image_create_overlay`   | Create a qcow2 overlay from a base image.                           |
| `image_convert_to_qcow2` | Convert an image to qcow2.                                          |
| `image_list`             | List all managed images.                                            |
| `recipe_apply`           | Load a recipe (YAML/JSON) and start a VM.                           |
| `logs_from_agent`        | Retrieve logs from inside a running VM via the Guest Agent.         |
| `distro_list`            | List known distro profiles (ISO URLs, credentials, notes).          |
| `iso_fetch`              | Download + sha256-verify a distro ISO into the cache.               |
| `vm_test_script`         | One-shot "test my script in distro X" → structured report + run_id. |
| `report_list`            | List all recorded runs (newest first).                              |
| `report_get`             | Retrieve one run's full markdown report.                            |

The headline tool is `vm_test_script`:

```json
{ "distro": "alpine", "script": "echo hello; uname -a" }
```

Register the server with any MCP client (see `INSTALLATION.md` §6 for ready-made snippets):

```json
{
  "mcpServers": {
    "qemu-ephemeral": {
      "command": "/abs/path/target/release/mcp-qemu-mcp-server",
      "args": []
    }
  }
}
```

## Documentation

- `INSTALLATION.md` — agent-followable setup: prerequisites, build, verify, MCP registration, smoke test, troubleshooting.
- `DESIGN.md` — crate layout, dependency rationale, dual-mode architecture, key design decisions.
- `examples/` — recipe examples (live-ISO and base-image shapes) and how recipes double as run records.
- All public APIs have doc comments (`///` and `//!`).

## Supported guests

| Distro              | Variants                         | Arches           | Notes                                                              |
| ------------------- | -------------------------------- | ---------------- | ------------------------------------------------------------------ |
| Alpine Linux 3.22.7 | `virt` (recommended), `standard` | x86_64, aarch64* | root/no-password live login; busybox `sh`; network down by default |

\* aarch64 requires UEFI (edk2) firmware installed; x86_64 (SeaBIOS) needs nothing extra. Live-ISO boots under pure TCG emulation take 2–4 minutes — the default boot timeout is 600s.

## License

MIT.
