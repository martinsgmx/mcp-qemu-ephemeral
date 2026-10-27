//! Binary entry point for the mcp-qemu-ephemeral MCP server.
//!
//! Runs the MCP service over stdio (JSON-RPC): an MCP client spawns this
//! binary, speaks MCP over stdin/stdout, and gets the full tool set
//! (lifecycle, images, recipes, distro profiles, one-shot script testing,
//! and run reports).
//!
//! Register it with any MCP client, e.g.:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "qemu-ephemeral": {
//!       "command": "/path/to/mcp-qemu-mcp-server",
//!       "args": []
//!     }
//!   }
//! }
//! ```

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    mcp_qemu_mcp_server::serve_stdio().await
}
