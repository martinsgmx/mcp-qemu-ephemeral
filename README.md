# mcp-qemu-ephemeral

A well-documented, dual-mode (CLI + MCP server) Rust project that manages the full lifecycle of isolated QEMU virtual machines.

## Overview

`mcp-qemu-ephemeral` lets you spawn, control, and tear down isolated QEMU VMs, manage disk images (base images, qcow2 overlays, ISO attachments), define and apply recipes, and retrieve logs from *inside* a running VM via the QEMU Guest Agent (qapi-rs).

It is usable in two modes from the same codebase:

1. **CLI**: `mcp-qemu-ephemeral run sandbox.sh` — spawn a VM, run a script, stream output, tear down cleanly.
2. **MCP server**: a stdio MCP server (kuri) exposing the same capabilities as tools.

| Crate | Purpose |
|-------|---------|
| `core` | QEMU process lifecycle management (`QemuHandle`, `QemuConfig`). |
| `image-manager` | Disk image provisioning (base, qcow2, iso). |
| `recipe` | Recipe parsing (YAML/JSON) and VM config application. |
| `mcp-server` | Kuri-based MCP server exposing lifecycle/image/recipe/log tools. |
| `cli` | CLI subcommands (`run`, `image`, `recipe`, `logs`). |

## Architecture

### Process management

QEMU is spawned as a child process via [`tokio::process`] with piped stdout/stderr so we can stream the VM's serial console output. The [`QemuHandle`] owns the child process and guarantees cleanup on `Drop` (no zombie QEMU processes).

- **spawn**: builds qemu args (`-nographic -serial stdio -monitor devnull`,
  `-drive`, `-cdrom`, `-m`, `-smp`, and optionally `-enable-agent` for the
  Guest Agent socket) and spawns with `std::process::Stdio::piped()`.
- **pause/resume**: SIGSTOP/SIGCONT via `libc::kill`.
- **stop**: SIGTERM with a grace period, then `kill()` (SIGKILL) as fallback.
- **kill**: immediate SIGKILL.
- **wait**: `child.wait()`.
- **read_line**: manual byte-by-byte read until `\n` or EOF (tokio's
  `ChildStdout` has no `read_line`).
- **stream_output**: drains stdout and stderr to a writer.
- **Drop**: best-effort `kill()` to avoid leaking processes.
- **Guest Agent socket**: QEMU can export a Guest Agent unix socket via
  `-enable-agent unix:<path>`; the path is stored on `QemuHandle` so callers
  (CLI or MCP server) can connect to it.

### Images

- Base image: a qcow2/raw disk with an installed OS.
- qcow2 overlay: copy-on-write file at `<images_dir>/<name>.qcow2`, created via `qemu-img create -f qcow2 -b <base> <overlay>`.
- iso: attached as a virtual CD-ROM via `-cdrom`.

### Recipes

Recipes are YAML or JSON documents describing a VM's configuration:

```yaml
image: /data/images/ubuntu.qcow2
memory_mib: 2048
cpus: 2
command: /bin/bash
args:
  - "-c"
  - "echo hello"
env:
  - [FOO, bar]
qemu_args: []
```

The recipe system parses, validates (image exists, memory/CPU bounds, non-empty command), and applies the recipe to spin up a VM via `QemuConfig`.

### Guest Agent logs

To retrieve logs from *inside* a running VM, the crate connects to the VM's Guest Agent unix socket and runs commands (e.g. `cat /var/log/syslog`) via the Guest Agent protocol. This requires the guest to run `qemu-guest-agent` (Ubuntu minimal/resolute images ship it). As a fallback, QEMU's own stdout/stderr (serial console) can be streamed.

## Building

```bash
cargo build
cargo build --release
cargo test
```

All crates compile with `--cfg tracing_unstable` because kuri pins tracing with the `valuable` feature, which is unstable on stable Rust. This is configured in `.cargo/config.toml`:

```toml
[build]
rustflags = ["--cfg", "tracing_unstable"]
```

## CLI usage

### Run a script in an isolated VM

```bash
./target/debug/mcp-qemu-ephemeral run sandbox.sh
```

This spawns a QEMU VM with the default config (1 GiB memory, 2 CPUs), attaches `sandbox.sh` (or runs a built-in test command), streams the VM's serial console output to stdout, and tears down the VM cleanly when the script exits.

### Image subcommand

```bash
./target/debug/mcp-qemu-ephemeral image
```

Not yet implemented in depth (see roadmap).

## MCP server usage

The MCP server runs over stdio and exposes 10 tools:

| Tool | Description |
|------|-------------|
| `qemu_start` | Start a QEMU VM from a JSON config; returns the VM PID. |
| `qemu_stop` | Stop a running VM by PID. |
| `qemu_pause` | Pause a VM by PID (SIGSTOP). |
| `qemu_resume` | Resume a VM by PID (SIGCONT). |
| `qemu_output` | Stream stdout/stderr from a VM by PID until exit. |
| `image_create_overlay` | Create a qcow2 overlay from a base image. |
| `image_convert_to_qcow2` | Convert an image to qcow2. |
| `image_list` | List all managed images. |
| `recipe_apply` | Load a recipe (YAML/JSON) and start a VM. |
| `logs_from_agent` | Retrieve logs from inside a running VM via the Guest Agent. |

To use an MCP client, pipe the stdio server:

```bash
./target/debug/mcp-qemu-mcp-server
```

An MCP client connects over stdin/stdout and can call `tools/list` then `tools/call` with any of the tools above.

## Documentation

- `DESIGN.md` — crate layout, dependency rationale, dual-mode architecture, key design decisions.
- All public APIs have doc comments (`///` and `//!`).
- README (this file) — architecture overview, usage examples.

## Roadmap / out of scope

In scope (per design):
- QEMU process lifecycle (spawn, wait, signal, cleanup) via `tokio::process`.
- Disk images: base, qcow2, iso.
- Recipes (YAML/JSON) defining image, memory, CPUs, script, env.
- Log retrieval from inside the VM via qapi-rs Guest Agent (QGA).
- Dual-mode binary: CLI subcommands AND an MCP server (stdio transport).
- Reference guest image: Ubuntu minimal/resolute.

Out of scope:
- Full hypervisor/virtualization layer beyond QEMU.
- Network bridging beyond a default user-mode network.
- Other guest OSes besides Ubuntu minimal as the primary reference.
- Long-running daemon mode (server is short-lived per stdio session).

## Contributing

- All crates are part of a Cargo workspace.
- `cargo test` must pass (0 failures).
- `cargo build --release` should succeed with zero warnings.
- Public APIs must be documented; new features must be added to `DESIGN.md`.

## License

MIT.
