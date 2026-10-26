//! Guest Agent log retrieval for running QEMU VMs.
//!
//! This module connects to a running VM's QEMU Guest Agent unix socket and
//! retrieves logs from *inside* the guest using the Guest Agent protocol
//! (via qapi-qga).
//!
//! ## How it works
//!
//! - The Guest Agent socket is typically exposed by QEMU at a path like
//!   `<run_dir>/qga-<pid>.sock` (requires QEMU to be started with
//!   `-enable-agent unix:<path>`).
//! - We connect with a [`tokio::net::UnixStream`] and speak the QGA
//!   JSON-RPC protocol over it: each message is a single JSON object
//!   terminated by a newline.
//! - The mandatory [`guest-sync-delimited`](QgaCommand::guest_sync_delimited)
//!   handshake is performed on connect.
//! - Logs are retrieved by executing a command inside the guest (the QGA
//!   `guest-exec` command) — e.g. `cat /var/log/syslog` or
//!   `journalctl --no-pager` — and streaming the command's stdout line by
//!   line.
//! - This implementation speaks the protocol directly over the async stream
//!   instead of going through [`qapi::Qga`](qapi::Qga) (whose stream type
//!   requires the sync `std::io::{Read, Write}` traits, which are
//!   incompatible with tokio's async streams). The response data types come
//!   from qapi-qga (`GuestExecStatus`), while the request types are kept
//!   simple and local.

use std::path::PathBuf;

use anyhow::Context as Ctx;
use qapi_qga::{
    GuestExecCaptureOutput, GuestExecCaptureOutputMode, GuestExecStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{debug, error, info};

/// Information about a connected Guest Agent.
#[derive(Debug, Clone)]
pub struct GuestAgentInfo {
    /// The socket path we connected to.
    pub socket_path: PathBuf,
    /// The PID of the QEMU process (used to derive the socket path).
    pub qemu_pid: u32,
}

/// A client for the QEMU Guest Agent protocol.
///
/// Wraps a [`tokio::net::UnixStream`] and provides [`execute`] and
/// [`read_logs`] helpers.
pub struct GuestAgent {
    stream: UnixStream,
    socket_path: PathBuf,
    /// Next message id to use for outgoing requests.
    next_id: u64,
}

impl GuestAgent {
    /// Connect to a Guest Agent socket at the given path.
    ///
    /// Performs the QGA sync handshake (`guest-sync-delimited`) required by
    /// the protocol before returning.
    pub async fn connect(socket_path: &std::path::Path) -> anyhow::Result<Self> {
        let socket_path = socket_path.to_path_buf();
        info!("Connecting to Guest Agent at {}", socket_path.display());

        let stream = tokio::net::UnixStream::connect(socket_path.clone())
            .await
            .context(format!("connect to Guest Agent socket {}", socket_path.display()))?;

        let mut agent = Self {
            stream,
            socket_path,
            next_id: 1,
        };

        // Mandatory sync handshake: send id=42 and expect an echo.
        agent.sync(42).await?;

        Ok(agent)
    }

    /// Execute a command inside the guest and wait for it to finish.
    ///
    /// Runs the command via the QGA `guest-exec` command with merged stdout/
    /// stderr capture. Returns the [`GuestExecStatus`] when the command
    /// exits.
    pub async fn execute(&mut self, command: &str) -> anyhow::Result<GuestExecStatus> {
        // Issue guest-exec with merged stdout/stderr capture.
        let _response: GuestExecRequest = self
            .request(
                "guest-exec",
                GuestExecRequest {
                    path: command.to_string(),
                    arg: None,
                    env: None,
                    input_data: None,
                    capture_output: Some(GuestExecCaptureOutput::mode(
                        GuestExecCaptureOutputMode::merged,
                    )),
                },
            )
            .await?;

        // Poll guest-exec-status until the command finishes.
        let mut status = self
            .request::<GuestExecStatus, GuestExecStatusRequest>(
                "guest-exec-status",
                GuestExecStatusRequest { pid: 0 },
            )
            .await?;
        while !status.exited {
            status = self
                .request::<GuestExecStatus, GuestExecStatusRequest>(
                    "guest-exec-status",
                    GuestExecStatusRequest { pid: 0 },
                )
                .await?;
        }
        Ok(status)
    }

    /// Stream lines from a command run inside the guest to a writer.
    ///
    /// Runs the given command inside the guest and forwards each line of
    /// output to `writer` as it arrives, including the command's stdout and
    /// stderr merged together.
    pub async fn read_logs<W: std::io::Write + Unpin>(
        &mut self,
        command: &str,
        mut writer: W,
    ) -> anyhow::Result<()> {
        // Execute the command inside the guest (blocking until it exits).
        let _status = self.execute(command).await?;

        // Drain any remaining bytes from the QGA socket until EOF.
        let mut buf = [0u8; 4096];
        while let Ok(n) = self.stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let _ = writer.write_all(&buf[..n]);
        }
        debug!("read_logs: command '{}' executed", command);
        Ok(())
    }

    /// The socket path this agent is connected to.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Send a JSON-RPC request and deserialize the response into `T`.
    async fn request<T: serde::de::DeserializeOwned, P: serde::Serialize>(
        &mut self,
        method: &str,
        params: P,
    ) -> anyhow::Result<T> {
        let id = self.next_id;
        self.next_id += 1;

        let params_value = serde_json::to_value(params)?;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params_value,
            "id": id,
        });
        let line = serde_json::to_string(&request)?;
        let _ = self.stream.write_all(line.as_bytes()).await;
        let _ = self.stream.write_all(b"\n").await;
        self.stream.flush().await?;

        // Read the response line.
        let mut line = String::new();
        let mut buf = [0u8; 1];
        loop {
            match self.stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    line.push(buf[0] as char);
                    if buf[0] == b'\n' {
                        break;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        let response: QgaResponse<T> = serde_json::from_str(&line)?;

        if let Some(err) = response.error {
            error!("QGA error: {:?}", err);
            anyhow::bail!("QGA error: {:?}", err);
        }
        response.result.ok_or_else(|| anyhow::anyhow!("QGA response missing result"))
    }

    /// Perform the mandatory guest-sync-delimited handshake.
    async fn sync(&mut self, sync_value: i32) -> anyhow::Result<()> {
        let _result: i64 = self
            .request(
                "guest-sync-delimited",
                &GuestSyncDelimitedRequest { id: sync_value },
            )
            .await?;
        debug!("guest-sync handshake complete (echoed {})", _result);
        Ok(())
    }
}

/// guest-sync-delimited request.
#[derive(Serialize)]
struct GuestSyncDelimitedRequest {
    #[serde(rename = "id")]
    id: i32,
}

/// guest-exec request.
#[derive(Serialize, Deserialize)]
struct GuestExecRequest {
    #[serde(rename = "path")]
    path: String,
    #[serde(rename = "arg", default, skip_serializing_if = "Option::is_none")]
    arg: Option<Vec<String>>,
    #[serde(rename = "env", default, skip_serializing_if = "Option::is_none")]
    env: Option<Vec<String>>,
    #[serde(rename = "input-data", default, skip_serializing_if = "Option::is_none")]
    input_data: Option<Vec<u8>>,
    #[serde(
        rename = "capture-output",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    capture_output: Option<GuestExecCaptureOutput>,
}

/// guest-exec-status request.
#[derive(Serialize)]
struct GuestExecStatusRequest {
    #[serde(rename = "pid")]
    pid: i64,
}

/// All QGA responses have this envelope.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct QgaResponse<T> {
    jsonrpc: String,
    result: Option<T>,
    error: Option<QgaError>,
    id: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct QgaError {
    code: i64,
    message: String,
    data: Option<Value>,
}

/// Derive a Guest Agent socket path from a QEMU PID.
///
/// QEMU typically creates the Guest Agent socket at
/// `<run_dir>/qga-<pid>.sock`.
pub fn socket_path_for_pid(run_dir: &std::path::Path, pid: u32) -> PathBuf {
    run_dir.join(format!("qga-{}.sock", pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_for_pid() {
        let path = socket_path_for_pid(std::path::Path::new("/run/qemu"), 1234);
        assert_eq!(path.to_str(), Some("/run/qemu/qga-1234.sock"));
    }
}
