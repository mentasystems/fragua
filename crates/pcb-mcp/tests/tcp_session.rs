//! End-to-end TCP smoke test: spin up the server, connect a TCP client,
//! drive a representative session, assert the project state was mutated.

use pcb_core::Project;
use pcb_mcp::McpServer;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn tcp_session_drives_project_mutations() {
    let project = Project::new("test");
    let server = McpServer::new(project.clone());

    // Bind to an ephemeral port so parallel test runs don't collide.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // release; the server will rebind. Simpler than threading the listener through.

    tokio::spawn(async move {
        // Tiny retry: the OS can briefly refuse rebind on the same port.
        for _ in 0..5 {
            if server.clone().run_tcp(&addr.to_string()).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // Connect, with a small grace period for the server to start.
    let stream = wait_for_connect(&addr.to_string()).await;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    send(&mut writer, &json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    })).await;
    let init: Value = recv(&mut lines).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "pcb");

    send(&mut writer, &json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {
            "name": "placement.add",
            "arguments": {
                "reference": "U1",
                "value": "MCU",
                "library": "Package_QFP:LQFP-32_7x7mm_P0.8mm",
                "x_mm": 25.0,
                "y_mm": 25.0,
                "pads": [
                    {"number":"1","x_mm":-3.0,"y_mm":0.0,"w_mm":0.6,"h_mm":1.5},
                    {"number":"2","x_mm":3.0, "y_mm":0.0,"w_mm":0.6,"h_mm":1.5}
                ]
            }
        }
    })).await;
    let placed: Value = recv(&mut lines).await;
    assert!(placed["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("Placed U1"));

    // The project handle was cloned into the server task, so this
    // assertion proves the session mutated the same Project the host
    // would expose to the UI.
    assert_eq!(project.read().board().footprints.len(), 1);
}

async fn wait_for_connect(addr: &str) -> TcpStream {
    timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(s) = TcpStream::connect(addr).await {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("mcp tcp server did not come up")
}

async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    w.write_all(&bytes).await.unwrap();
    w.flush().await.unwrap();
}

async fn recv<R: tokio::io::AsyncBufRead + Unpin>(r: &mut tokio::io::Lines<R>) -> Value {
    let line = timeout(Duration::from_secs(2), r.next_line())
        .await
        .expect("server reply timed out")
        .unwrap()
        .expect("server closed connection");
    serde_json::from_str(&line).unwrap()
}
