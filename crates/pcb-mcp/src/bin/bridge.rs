//! stdio ↔ TCP bridge.
//!
//! The Tauri app hosts the MCP server on TCP so its UI and the agent
//! share one `Project`. Claude Code only speaks stdio MCP, so we ship
//! this tiny relay: launched by Claude as the MCP "binary", it forwards
//! stdin → TCP → stdout transparently.
//!
//! Survives app restarts: if the initial connect fails or the TCP
//! socket later drops, the bridge keeps stdin alive and retries the
//! connection underneath. As long as the user doesn't kill `fragua`
//! for longer than the retry window, Claude Code never sees the MCP
//! disconnect — equivalent to how the Pencil binary stays connected
//! across desktop-app restarts.
//!
//! Usage (in Claude Code config): `pcb-mcp-bridge [host:port]`.
//! Default address is `127.0.0.1:7878`.

use std::env;
use std::time::{Duration, Instant};

use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// How long we wait at startup for `fragua` to come up before giving
/// up (the bridge process exits and Claude Code reports the MCP as
/// failed). Generous enough that "claude mcp list" + "open the app"
/// race tolerantly.
const INITIAL_CONNECT_SECS: u64 = 60;

/// How long we keep retrying after the TCP socket drops mid-session
/// (typically: user rebuilt and restarted `fragua`). Long enough to
/// span a full release `cargo build` + restart.
const RECONNECT_SECS: u64 = 180;

/// Sleep between reconnect attempts.
const RECONNECT_BACKOFF_MS: u64 = 500;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let addr = env::args().nth(1).unwrap_or_else(|| "127.0.0.1:7878".into());

    let mut stdin = BufReader::new(stdin());
    let mut stdout = stdout();

    let Some(mut stream) = connect_with_retry(&addr, INITIAL_CONNECT_SECS).await else {
        eprintln!(
            "[pcb-bridge] timed out connecting to {addr} after {INITIAL_CONNECT_SECS}s"
        );
        return Ok(());
    };

    'session: loop {
        let (tcp_r, mut tcp_w) = stream.into_split();
        let mut tcp_r = BufReader::new(tcp_r);
        let mut up_buf = String::new();
        let mut down_buf = String::new();

        loop {
            up_buf.clear();
            down_buf.clear();
            tokio::select! {
                read = stdin.read_line(&mut up_buf) => match read {
                    Ok(0) => return Ok(()), // stdin EOF — Claude Code shut us down.
                    Err(_) => return Ok(()),
                    Ok(_) => {
                        if tcp_w.write_all(up_buf.as_bytes()).await.is_err() {
                            break; // TCP write failed, fall through to reconnect.
                        }
                        let _ = tcp_w.flush().await;
                    }
                },
                read = tcp_r.read_line(&mut down_buf) => match read {
                    Ok(0) => break,        // server closed.
                    Err(_) => break,
                    Ok(_) => {
                        let _ = stdout.write_all(down_buf.as_bytes()).await;
                        let _ = stdout.flush().await;
                    }
                },
            }
        }

        eprintln!("[pcb-bridge] connection to {addr} dropped, reconnecting…");
        match connect_with_retry(&addr, RECONNECT_SECS).await {
            Some(s) => {
                eprintln!("[pcb-bridge] reconnected to {addr}");
                stream = s;
                continue 'session;
            }
            None => {
                eprintln!(
                    "[pcb-bridge] reconnect window of {RECONNECT_SECS}s exhausted, exiting"
                );
                return Ok(());
            }
        }
    }
}

async fn connect_with_retry(addr: &str, secs: u64) -> Option<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut printed = false;
    loop {
        match TcpStream::connect(addr).await {
            Ok(s) => return Some(s),
            Err(_) => {
                if !printed {
                    eprintln!("[pcb-bridge] waiting for {addr}…");
                    printed = true;
                }
                if Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(RECONNECT_BACKOFF_MS)).await;
            }
        }
    }
}
