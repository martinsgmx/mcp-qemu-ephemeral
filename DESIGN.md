# Design Document — mcp-qemu-ephemeral

## Overview

`mcp-qemu-ephemeral` is a dual-mode (CLI + MCP server) Rust application that manages the full lifecycle of isolated QEMU virtual machines. It is built around three concepts:

1. **VM lifecycle** — spawning, controlling, and tearing down a QEMU process via `tokio::process`.
2. **Image management** — provisioning base images, qcow2 conversion, and attaching/detaching qcow and iso media.
3. **Recipes** — declarative YAML/JSON documents describing a VM's configuration (image, memory, CPUs, command, environment).

The same Rust core powers both the CLI (`mcp-qemu-ephemeral run sandbox.sh`) and the MCP server (kuri over stdio), exposing identical capabilities as MCP tools.

## Crate layout (workspace)

| Crate | Purpose |
|-------|---------|
| `core` | QEMU process lifecycle management (`QemuHandle`, `QemuConfig`). |
| `image-manager` | Disk image provisioning, qcow2 conversion, attach/detach. |
| `recipe` | Recipe parsing (YAML/JSON) and validation. |
| `mcp-server` | Kuri-based MCP server exposing lifecycle/image/recipe/log tools. |
| `cli` | CLI subcommands (`run`, `image`, `recipe`, `logs`). |

## Dependency rationale

- **tokio** — async runtime; `tokio::process` for spawning/monitoring QEMU.
- **qapi-rs** — QMP and Guest Agent protocol for talking to a running VM's Guest Agent (log retrieval).
- **kuri** — ergonomic MCP server framework (stdio transport), chosen for its minimal macros and `tower`-based middleware.
- **serde / serde_yaml** — recipe parsing.
- **clap** — CLI argument parsing.
- **anyhow / tracing** — error handling and logging.

## Dual-mode architecture

The CLI and MCP server share the same core modules (`core`, `image-manager`, `recipe`). Thin adapters convert between:
- CLI: clap subcommands → core module functions.
- MCP: kuri tools → core module functions.

This ensures consistent behavior and a single source of truth for VM management.

## Key design decisions

### Process management via `tokio::process`

QEMU is spawned as a child process with piped stdout/stderr so we can stream the VM's serial console output. The `QemuHandle` owns the `tokio::process::Child` and guarantees cleanup on `Drop` (no zombie QEMU processes). Pause/resume uses `SIGSTOP`/`SIGCONT` via `libc::kill`. Shutdown uses `SIGTERM` with a grace period, then `SIGKILL`.

### Log retrieval via qapi-rs Guest Agent

To retrieve logs from *inside* a running VM, we connect to the VM's Guest Agent unix socket over QGA and stream log output. This requires the guest to run the `qemu-guest-agent` service (Ubuntu minimal/resolute images ship it). As a fallback, QEMU's own stdout/stderr (serial console) can be streamed.

### Images

- Base image: a raw/qcow2 disk with an installed OS (Ubuntu minimal/resolute).
- qcow2: copy-on-write overlay to avoid mutating the base image.
- iso: attached as a CD-ROM for installing or running a script.

## Security / safety boundaries

- QEMU runs user-mode (no root required for basic operation). A user-mode network is used.
- The `QemuHandle` guarantees process cleanup on drop to prevent leaks.
- Recipes are validated before applying (image exists, memory/CPUs within sane bounds).

## File locations

- `DESIGN.md` — this document.
- `README.md` — usage guide for CLI and MCP modes.
- `core/src/lib.rs` — QEMU lifecycle.
- `image-manager/src/lib.rs` — image management.
- `recipe/src/lib.rs` — recipe parsing.
- `mcp-server/src/` — MCP server tools.
- `cli/src/main.rs` — CLI entry point.
