# INSTALLATION.md — setup instructions for humans and AI agents

This document is written to be **followed end-to-end by an AI coding agent**
(Claude Code, OpenCode, Cursor, Codex, Copilot Workspace, …) with no human
intervention. Every step is idempotent — running it twice is safe.

The standard agent prompt is:

> Fetch and read <repo_url>/INSTALLATION.md, then follow the setup
> instructions on this machine. Verify each step before moving to the next.
> Report any step that fails and stop.

---

## 1. Prerequisites

Check each tool; install what is missing before continuing.

| Requirement | Check | Notes |
|---|---|---|
| Rust ≥ 1.75 (stable) | `rustc --version && cargo --version` | https://rustup.rs |
| `qemu-system-x86_64` | `command -v qemu-system-x86_64` | Arch: `pacman -S qemu-full` · Debian/Ubuntu: `apt install qemu-system-x86 qemu-utils` · Fedora: `dnf install qemu-kvm` |
| `qemu-system-aarch64` | `command -v qemu-system-aarch64` | Same packages as above (needed only for ARM guests) |
| `qemu-img` | `command -v qemu-img` | Ships with the packages above |
| `curl` or `wget` | `command -v curl || command -v wget` | For ISO downloads |
| `sha256sum` | `command -v sha256sum` | coreutils; present on all Linux distros |

Optional (improves boot speed ~20x, falls back to TCG emulation without it):
`/dev/kvm` present and accessible (`ls -l /dev/kvm`).

**Do not** install UEFI/edk2 firmware unless you intend to use aarch64
guests; the x86_64 path is SeaBIOS-based and needs nothing extra.

## 2. Get the code

```bash
git clone <repo_url> mcp-qemu-ephemeral
cd mcp-qemu-ephemeral
```

(Skip if the directory already exists locally — `cd` into it instead.)

## 3. Build

```bash
cargo build --release
```

Produces two binaries:

- `target/release/mcp-qemu-ephemeral` — the CLI
- `target/release/mcp-qemu-mcp-server` — the stdio MCP server

Build must finish with **no errors**. Warnings are acceptable but should be
reported.

## 4. Verify

Run the full test suite — all crates must report zero failures:

```bash
cargo test --workspace
```

Then confirm the MCP server answers an MCP handshake (should print a JSON
line containing `"serverInfo":{"name":"mcp-qemu-ephemeral"...`):

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"check","version":"0"}}}' \
  | ./target/release/mcp-qemu-mcp-server
```

## 5. Smoke test (first real run)

Download the Alpine ISO (~65 MB, sha256-verified) and run a script inside a
real VM. The full flow is: ISO fetch → live boot → auto-login on the serial
console → script run → teardown → report.

```bash
printf '#!/bin/sh\necho hello-from-alpine\nuname -a\n' > /tmp/mcpq-smoke.sh
./target/release/mcp-qemu-ephemeral test --distro alpine /tmp/mcpq-smoke.sh
```

Expected: a markdown report ending in `run_id: …` with **Status: success**
and the script's output in the console tail. The CLI exits 0.

Timing note: without `/dev/kvm` (pure TCG emulation), the boot-to-login
phase takes **2–4 minutes** — this is normal; the boot timeout is 600s.
With KVM it is a few seconds.

Artifacts land in `runs/<run_id>/`:
`recipe.yaml` (exact VM assembly), `script.sh`, `serial.log` (full console
transcript), `report.md`, `report.json`.

## 6. Register the MCP server with a client

All clients use the same shape: a stdio command pointing at the built
binary (use its **absolute path**). Pick the matching config file:

- **Generic / Claude Desktop** — `claude_desktop_config.json`:

  ```json
  {
    "mcpServers": {
      "qemu-ephemeral": {
        "command": "<abs_path>/target/release/mcp-qemu-mcp-server",
        "args": []
      }
    }
  }
  ```

- **opencode** — add to `~/.config/opencode/opencode.json`:

  ```json
  {
    "mcp": {
      "qemu-ephemeral": {
        "type": "local",
        "command": ["<abs_path>/target/release/mcp-qemu-mcp-server"]
      }
    }
  }
  ```

- **Cursor / Windsurf** — same generic shape in their `mcp.json`.

After registering, ask the client for the tool list; it must include
`vm_test_script`, `distro_list`, `iso_fetch`, `report_list`, `report_get`
among 15 tools total.

## 7. Agent usage cookbook

Once installed (CLI or MCP), the canonical one-shot flow for
*"test my bash script in linux alpine"*:

- **CLI**: `./target/release/mcp-qemu-ephemeral test --distro alpine my-script.sh`
- **MCP**: call `vm_test_script` with
  `{"distro": "alpine", "script": "<shell text>"}`, then `report_get` with
  the returned `run_id` if the summary is not enough.

Call `distro_list` first when unsure what distros/variants/arches are
supported — it returns ISO URLs, login credentials, and per-profile notes
(e.g. Alpine live ISOs: root has no password, network is down until
`setup-interfaces -a -r`, scripts run under busybox `/bin/sh`).

## 8. Common failures and fixes

| Symptom | Cause | Fix |
|---|---|---|
| `boot failure … 'devnull' is not a valid char driver` | stale binary from an old build | `cargo build --release` again |
| `boot failure` with empty console output | ISO not yet downloaded, or TCG boot still in progress | check `images/isos/`; wait, or raise `boot_timeout_secs` |
| `checksum mismatch for …` | corrupted/partial ISO in `images/isos/` | delete the ISO file and rerun |
| `no profile for distro "x"` | distro not in the registry | call `distro_list` to see supported distros |
| `neither curl nor wget is installed` | no HTTP helper on PATH | install curl |
| aarch64 run fails | UEFI firmware not installed | install `edk2-aarch64` firmware package, or use `--arch x86_64` |

## 9. Done criteria

Setup is complete when ALL of the following hold:

1. `cargo test --workspace` — zero failures.
2. Both binaries exist in `target/release/`.
3. The MCP handshake check above prints `serverInfo`.
4. The smoke test in §5 prints `Status: success` (or, on a machine without
   KVM, completes within the 600s boot window).
