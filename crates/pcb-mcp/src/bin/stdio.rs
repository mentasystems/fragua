//! Headless stdio MCP server. Useful as a smoke test target and for
//! plugging the agent into pcb without launching the full Tauri app.

use pcb_core::Project;
use pcb_mcp::McpServer;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let project = Project::new("scratch");
    McpServer::new(project).run_stdio().await
}
