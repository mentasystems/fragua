//! stdio ↔ TCP bridge.
//!
//! The Tauri app hosts the MCP server on TCP so its UI and the agent
//! share one `Project`. Claude Code only speaks stdio MCP, so we ship
//! this tiny relay: launched by Claude as the MCP "binary", it forwards
//! stdin → TCP → stdout transparently.
//!
//! Usage (in Claude Code config): `pcb-mcp-bridge [host:port]`.
//! Default address is `127.0.0.1:7878`.

use std::env;

use tokio::io::{copy, stdin, stdout, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let addr = env::args().nth(1).unwrap_or_else(|| "127.0.0.1:7878".into());
    let stream = TcpStream::connect(&addr).await?;
    let (mut tcp_r, mut tcp_w) = stream.into_split();

    let mut stdin = stdin();
    let mut stdout = stdout();

    let up = async {
        let _ = copy(&mut stdin, &mut tcp_w).await;
        let _ = tcp_w.shutdown().await;
    };
    let down = async {
        let _ = copy(&mut tcp_r, &mut stdout).await;
        let _ = stdout.flush().await;
    };

    tokio::select! {
        () = up => {},
        () = down => {},
    }
    Ok(())
}
