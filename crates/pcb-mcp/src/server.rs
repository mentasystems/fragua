//! MCP server core. Reads JSON-RPC frames line-by-line from a transport
//! (stdin) and dispatches them.

use pcb_core::Project;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::protocol::{error_code, Notification, Request, Response, PROTOCOL_VERSION};
use crate::tools;

/// Server bound to a project. Cloning is cheap; pass clones into spawn'd
/// tasks freely.
#[derive(Clone)]
pub struct McpServer {
    project: Project,
}

impl McpServer {
    #[must_use]
    pub fn new(project: Project) -> Self {
        Self { project }
    }

    /// Run the server on stdin/stdout until EOF on stdin.
    ///
    /// Requests are handled sequentially — the project state is behind a
    /// single `RwLock`, and spawning per-request adds nothing but
    /// shutdown races. Tools return quickly enough that head-of-line
    /// blocking is fine for now.
    pub async fn run_stdio(self) -> std::io::Result<()> {
        self.serve_session(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run a TCP listener that accepts MCP clients on `addr`. Each client
    /// gets its own line-delimited JSON-RPC session, sharing the same
    /// `Project` — so a Tauri-hosted server and a remote Claude Code
    /// instance see the same board.
    pub async fn run_tcp(self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        loop {
            let (sock, _peer) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                let (reader, writer) = sock.into_split();
                let _ = server.serve_session(reader, writer).await;
            });
        }
    }

    async fn serve_session<R, W>(&self, reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line) {
                let mut bytes = serde_json::to_vec(&reply).unwrap_or_default();
                bytes.push(b'\n');
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }

    fn handle_line(&self, line: &str) -> Option<Response> {
        let req: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(Response::err(
                    Value::Null,
                    error_code::PARSE_ERROR,
                    format!("parse error: {e}"),
                ));
            }
        };
        // Notifications (id absent) get no reply.
        let Some(id) = req.id.clone() else {
            return None;
        };
        Some(self.dispatch(&req, id))
    }

    fn dispatch(&self, req: &Request, id: Value) -> Response {
        match req.method.as_str() {
            "initialize" => Response::ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "pcb",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            ),
            "notifications/initialized" | "initialized" => {
                Response::ok(id, json!({}))
            }
            "tools/list" => Response::ok(
                id,
                json!({ "tools": tools::catalog() }),
            ),
            "tools/call" => self.handle_tool_call(req, id),
            "ping" => Response::ok(id, json!({})),
            other => Response::err(
                id,
                error_code::METHOD_NOT_FOUND,
                format!("method not implemented: {other}"),
            ),
        }
    }

    fn handle_tool_call(&self, req: &Request, id: Value) -> Response {
        let name = req
            .params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = req
            .params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match tools::dispatch(&self.project, name, &args) {
            Ok(value) => Response::ok(id, value),
            Err(err) => Response::err(id, err.code, err.message),
        }
    }
}

/// Build a server-sent notification frame. Useful when the host wants to
/// push log/activity events to the client; not yet wired up but kept here
/// so callers know the helper exists.
#[must_use]
pub fn activity_notification(level: &str, message: &str) -> Notification {
    Notification::new(
        "notifications/message",
        json!({ "level": level, "logger": "pcb", "data": message }),
    )
}
