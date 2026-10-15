//! Real binary/stdio checks: schemas and early rejection need no speaker LAN.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Session {
    fn start() -> Self {
        let data = std::env::temp_dir().join(format!(
            "fsonos-reviewed-undo-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let mut child = Command::new(env!("CARGO_BIN_EXE_fsonos"))
            .arg("mcp")
            .env("FSONOS_DATA_DIR", data)
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fsonos mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, value: &Value) {
        writeln!(self.stdin, "{value}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("bounded wait for MCP response");
            let value: Value = serde_json::from_str(&line).expect("JSON on MCP stdout");
            if value["id"] == id {
                return value;
            }
        }
    }

    fn initialize(&mut self) {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": { "name": "reviewed-undo-test", "version": "0" }
            }
        }));
        assert_eq!(self.response(1)["result"]["serverInfo"]["name"], "fsonos");
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn reviewed_undo_tools_are_advertised_without_scope_widening_arguments() {
    let mut session = Session::start();
    session.initialize();
    session.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = session.response(2);
    let tools = list["result"]["tools"].as_array().unwrap();
    let find = |name: &str| {
        let found: Vec<_> = tools.iter().filter(|tool| tool["name"] == name).collect();
        assert_eq!(found.len(), 1, "{name}: {list}");
        found[0]
    };
    assert_eq!(find("preview_undo")["annotations"]["readOnlyHint"], true);
    assert_eq!(find("preview_undo")["annotations"]["idempotentHint"], true);
    let checked = find("undo_checked");
    assert_ne!(checked["annotations"]["readOnlyHint"], true);
    assert_ne!(checked["annotations"]["idempotentHint"], true);
    assert_eq!(checked["inputSchema"]["required"], json!(["expected_id"]));
    let properties = checked["inputSchema"]["properties"].as_object().unwrap();
    assert_eq!(properties.len(), 1);
    assert!(properties.contains_key("expected_id"));
    find("undo_last");
}

#[test]
fn invalid_reviewed_action_is_a_tool_error_without_speaker_io() {
    let mut session = Session::start();
    session.initialize();
    session.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "undo_checked", "arguments": { "expected_id": 0 } }
    }));
    let reply = session.response(2);
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("INVALID_ARGUMENT"),
        "{reply}"
    );
    session.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "text": "still responsive" } }
    }));
    assert_eq!(
        session.response(3)["result"]["content"][0]["text"],
        "still responsive"
    );
}
