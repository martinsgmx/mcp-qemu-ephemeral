use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use kuri::context::Inject;
use kuri::MCPService;
use kuri::MCPServiceBuilder;
use kuri::ToolError;
use kuri_mcp_protocol::tool::{generate_tool_schema, ToolResult};
use mcp_qemu_core::{QemuConfig, QemuHandle};
use mcp_qemu_core::guest_agent::GuestAgent;
use mcp_qemu_image_manager::ImageManager;
use mcp_qemu_recipe::load_recipe;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Shared, mutable state injected into every tool handler.
#[derive(Default)]
pub struct ServerState {
    /// QEMU VMs indexed by PID.
    pub vms: HashMap<u32, QemuHandle>,
    /// Image manager.
    pub images: ImageManager,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            vms: HashMap::new(),
            images: ImageManager::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct QemuStartParams {
    config: Value,
}

#[derive(Deserialize, JsonSchema)]
struct QemuStopParams {
    pid: u32,
}

#[derive(Deserialize, JsonSchema)]
struct QemuPauseParams {
    pid: u32,
}

#[derive(Deserialize, JsonSchema)]
struct QemuResumeParams {
    pid: u32,
}

#[derive(Deserialize, JsonSchema)]
struct QemuOutputParams {
    pid: u32,
}

#[derive(Deserialize, JsonSchema)]
struct ImageCreateOverlayParams {
    name: String,
    base: String,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct ImageConvertToQcow2Params {
    src: String,
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct RecipeApplyParams {
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct LogsFromAgentParams {
    pid: u32,
    logpath: String,
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

/// Tool: start a QEMU VM from a config (JSON).
#[derive(Clone)]
struct QemuStart;

#[async_trait(?Send)]
impl kuri::ToolHandler for QemuStart {
    fn name(&self) -> &'static str {
        "qemu_start"
    }
    fn description(&self) -> &'static str {
        "Start a QEMU VM with the given configuration (JSON). Returns the VM PID."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<QemuStartParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: QemuStartParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        let config: QemuConfig = serde_json::from_value(params.config)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        // Spawn first (no lock needed), then insert under the lock.
        let handle = QemuHandle::spawn(config)
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        let pid = handle.pid();
        {
            let mut state = state.lock().unwrap();
            state.vms.insert(pid, handle);
        }
        Ok(serde_json::json!({ "pid": pid }))
    }
}

/// Tool: stop a running QEMU VM by PID.
#[derive(Clone)]
struct QemuStop;

#[async_trait(?Send)]
impl kuri::ToolHandler for QemuStop {
    fn name(&self) -> &'static str {
        "qemu_stop"
    }
    fn description(&self) -> &'static str {
        "Stop a running QEMU VM by PID."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<QemuStopParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: QemuStopParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        // Take ownership of the handle, then release the lock before awaiting.
        let mut handle = {
            let mut state = state.lock().unwrap();
            state.vms.remove(&params.pid)
        }
        .ok_or_else(|| {
            ToolError::InvalidParameters(format!("VM {} not found", params.pid))
        })?;
        handle
            .stop()
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

/// Tool: pause a QEMU VM by PID.
#[derive(Clone)]
struct QemuPause;

#[async_trait(?Send)]
impl kuri::ToolHandler for QemuPause {
    fn name(&self) -> &'static str {
        "qemu_pause"
    }
    fn description(&self) -> &'static str {
        "Pause a QEMU VM by PID (SIGSTOP)."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<QemuPauseParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: QemuPauseParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        // Pause is synchronous; take ownership, mutate, reinsert.
        {
            let mut state = state.lock().unwrap();
            let mut handle = state.vms.remove(&params.pid).ok_or_else(|| {
                ToolError::InvalidParameters(format!("VM {} not found", params.pid))
            })?;
            handle.pause().map_err(|e| ToolError::ExecutionError(e.to_string()))?;
            state.vms.insert(params.pid, handle);
        }
        Ok(serde_json::json!({ "ok": true }))
    }
}

/// Tool: resume a QEMU VM by PID.
#[derive(Clone)]
struct QemuResume;

#[async_trait(?Send)]
impl kuri::ToolHandler for QemuResume {
    fn name(&self) -> &'static str {
        "qemu_resume"
    }
    fn description(&self) -> &'static str {
        "Resume a QEMU VM by PID (SIGCONT)."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<QemuResumeParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: QemuResumeParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        {
            let mut state = state.lock().unwrap();
            let handle = state
                .vms
                .get_mut(&params.pid)
                .ok_or_else(|| {
                    ToolError::InvalidParameters(format!("VM {} not found", params.pid))
                })?;
            handle.resume().map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        }
        Ok(serde_json::json!({ "ok": true }))
    }
}

/// Tool: stream output from a QEMU VM by PID.
#[derive(Clone)]
struct QemuOutput;

#[async_trait(?Send)]
impl kuri::ToolHandler for QemuOutput {
    fn name(&self) -> &'static str {
        "qemu_output"
    }
    fn description(&self) -> &'static str {
        "Stream stdout/stderr from a QEMU VM by PID until the VM exits."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<QemuOutputParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: QemuOutputParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        // Take ownership of the handle, release the lock, then drain output.
        let mut handle = {
            let mut state = state.lock().unwrap();
            state.vms.remove(&params.pid)
        }
        .ok_or_else(|| {
            ToolError::InvalidParameters(format!("VM {} not found", params.pid))
        })?;
        let mut out = Vec::new();
        handle
            .stream_output(&mut out)
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        Ok(serde_json::Value::String(String::from_utf8_lossy(&out).to_string()))
    }
}

/// Tool: create a qcow2 overlay from a base image.
#[derive(Clone)]
struct ImageCreateOverlay;

#[async_trait(?Send)]
impl kuri::ToolHandler for ImageCreateOverlay {
    fn name(&self) -> &'static str {
        "image_create_overlay"
    }
    fn description(&self) -> &'static str {
        "Create a qcow2 overlay from a base image."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<ImageCreateOverlayParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: ImageCreateOverlayParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        let path = {
            let state = state.lock().unwrap();
            state
                .images
                .create_overlay(&params.name, &params.base, params.size)
                .await
                .map_err(|e| ToolError::ExecutionError(e.to_string()))?
        };
        Ok(serde_json::json!({ "path": path.to_str().unwrap_or("") }))
    }
}

/// Tool: convert an image to qcow2.
#[derive(Clone)]
struct ImageConvertToQcow2;

#[async_trait(?Send)]
impl kuri::ToolHandler for ImageConvertToQcow2 {
    fn name(&self) -> &'static str {
        "image_convert_to_qcow2"
    }
    fn description(&self) -> &'static str {
        "Convert an image to qcow2 format."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<ImageConvertToQcow2Params>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: ImageConvertToQcow2Params = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        let path = {
            let state = state.lock().unwrap();
            state
                .images
                .convert_to_qcow2(&params.src, &params.name)
                .await
                .map_err(|e| ToolError::ExecutionError(e.to_string()))?
        };
        Ok(serde_json::json!({ "path": path.to_str().unwrap_or("") }))
    }
}

/// Tool: list images.
#[derive(Clone)]
struct ImageList;

#[async_trait(?Send)]
impl kuri::ToolHandler for ImageList {
    fn name(&self) -> &'static str {
        "image_list"
    }
    fn description(&self) -> &'static str {
        "List all managed images."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<()>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        _params: Value,
    ) -> ToolResult<Value> {
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        let images = {
            let state = state.lock().unwrap();
            state.images.list().await
        }
        .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        Ok(serde_json::Value::Array(
            images
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "path": i.path.to_str().unwrap_or(""),
                        "format": &i.format,
                        "size": i.size,
                        "is_overlay": i.is_overlay,
                    })
                })
                .collect(),
        ))
    }
}

/// Tool: load and apply a recipe from a file path.
#[derive(Clone)]
struct RecipeApply;

#[async_trait(?Send)]
impl kuri::ToolHandler for RecipeApply {
    fn name(&self) -> &'static str {
        "recipe_apply"
    }
    fn description(&self) -> &'static str {
        "Load a recipe from a file (YAML/JSON) and start a VM."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<RecipeApplyParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: RecipeApplyParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);
        let recipe = load_recipe(std::path::Path::new(&params.path))
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        let config = recipe
            .apply()
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        let handle = QemuHandle::spawn(config)
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;
        let pid = handle.pid();
        {
            let mut state = state.lock().unwrap();
            state.vms.insert(pid, handle);
        }
        Ok(serde_json::json!({ "pid": pid, "recipe": params.path }))
    }
}

/// Tool: retrieve logs from inside a running VM via the Guest Agent.
#[derive(Clone)]
struct LogsFromAgent;

#[async_trait(?Send)]
impl kuri::ToolHandler for LogsFromAgent {
    fn name(&self) -> &'static str {
        "logs_from_agent"
    }
    fn description(&self) -> &'static str {
        "Retrieve logs from inside a running VM via the Guest Agent."
    }
    fn schema(&self) -> Value {
        generate_tool_schema::<LogsFromAgentParams>()
            .expect("Failed to generate tool schema")
    }
    async fn call(
        &self,
        context: &kuri::context::Context,
        params: Value,
    ) -> ToolResult<Value> {
        let params: LogsFromAgentParams = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParameters(e.to_string()))?;
        let state: Inject<Arc<Mutex<ServerState>>> =
            kuri::context::FromContext::from_context(context);

        // Verify the VM exists and obtain its Guest Agent socket path.
        let agent_socket = {
            let state = state.lock().unwrap();
            let entry = state.vms.get(&params.pid);
            if entry.is_none() {
                return Err(ToolError::InvalidParameters(format!(
                    "VM {} not found",
                    params.pid
                )));
            }
            entry
                .and_then(|h| h.agent_socket().map(PathBuf::from))
                .unwrap_or_else(|| std::path::PathBuf::from("/run/qemu/qga.sock"))
        };

        let mut agent = GuestAgent::connect(agent_socket.as_path())
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

        let command = format!("cat {}", params.logpath);
        let mut out = Vec::new();
        agent
            .read_logs(&command, &mut out)
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

        Ok(serde_json::Value::String(
            String::from_utf8_lossy(&out).to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Service builder
// ---------------------------------------------------------------------------

/// Build the MCP service with all tools registered.
pub fn build_mcp_service() -> MCPService {
    let state = Arc::new(Mutex::new(ServerState::new()));

    MCPServiceBuilder::new(
        "mcp-qemu-ephemeral".to_string(),
        "Manage isolated QEMU VMs (lifecycle, images, recipes, logs).".to_string(),
    )
    .with_tool(QemuStart)
    .with_tool(QemuStop)
    .with_tool(QemuPause)
    .with_tool(QemuResume)
    .with_tool(QemuOutput)
    .with_tool(ImageCreateOverlay)
    .with_tool(ImageConvertToQcow2)
    .with_tool(ImageList)
    .with_tool(RecipeApply)
    .with_tool(LogsFromAgent)
    .with_state(Inject::new(state))
    .build()
}

/// Serve the MCP service over stdio.
pub async fn serve_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let service = build_mcp_service();
    let reader = tokio::io::stdin();
    let writer = tokio::io::stdout();
    let transport = kuri::transport::ByteTransport::new(reader, writer);
    let server = kuri::Server::new(service);
    server.run(transport).await?;
    Ok(())
}
