//! `fsonos mcp` speaks MCP over stdio. Spawns the real binary, initializes a
//! session, lists the tools, round-trips `echo`, and checks that a control
//! tool the house policy denies answers a coded tool error (on the 2024-11-05
//! era) without touching the network.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Session {
    fn start() -> Self {
        Self::start_in(&std::env::temp_dir().join("fsonos-mcp-stdio-default"))
    }

    /// Start `fsonos mcp` with `data_dir` as its data directory.
    fn start_in(data_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fsonos"))
            .arg("mcp")
            .env("RUST_LOG", "warn")
            .env("FSONOS_DATA_DIR", data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn fsonos mcp");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
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

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").expect("write request");
        stdin.flush().expect("flush");
    }

    /// Next JSON-RPC message carrying `id` (skips notifications / log lines).
    fn response(&self, id: u64) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("server answered within 20s");
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                panic!("non-JSON on MCP stdout: {line}");
            };
            if msg["id"] == id {
                return msg;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn echo_tool_round_trips_over_stdio() {
    let mut s = Session::start();

    s.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "fsonos-test", "version": "0"}
        }
    }));
    let init = s.response(1);
    assert_eq!(init["result"]["serverInfo"]["name"], "fsonos", "{init}");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05", "{init}");
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = s.response(2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(tools.iter().any(|t| t["name"] == "echo"), "{list}");

    let text = "Ada\u{2019}s Studio \u{2014} hello";
    s.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "echo", "arguments": {"text": text}}
    }));
    let call = s.response(3);
    assert_ne!(call["result"]["isError"], true, "{call}");
    assert_eq!(call["result"]["content"][0]["text"], text, "{call}");
}

fn initialize(s: &mut Session) {
    s.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "fsonos-test", "version": "0"}
        }
    }));
    s.response(1);
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
}

#[test]
fn control_tools_are_advertised_with_their_arguments() {
    let mut s = Session::start();
    initialize(&mut s);
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = s.response(2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no {name} tool: {list}"))
    };
    for name in [
        "list_zones",
        "play",
        "pause",
        "resume",
        "next",
        "previous",
        "set_volume",
        "mute",
        "group",
        "ungroup",
        "dj_start",
        "dj_skip",
        "dj_stop",
    ] {
        let t = tool(name);
        assert!(
            t["description"].as_str().is_some_and(|d| d.len() > 20),
            "{t}"
        );
    }
    let volume = &tool("set_volume")["inputSchema"];
    for arg in ["zone", "volume", "delta", "group"] {
        assert!(
            volume["properties"].get(arg).is_some(),
            "set_volume lacks {arg}: {volume}"
        );
    }
    assert_eq!(volume["required"], json!(["zone"]), "{volume}");
    assert_eq!(
        tool("group")["inputSchema"]["required"],
        json!(["zone", "to"])
    );
    assert_eq!(
        tool("list_zones")["annotations"]["readOnlyHint"],
        true,
        "{list}"
    );
}

#[test]
fn a_denied_tool_answers_policy_denied_before_any_speaker_io() {
    let dir = std::env::temp_dir().join(format!("fsonos-mcp-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("data dir");
    std::fs::write(
        dir.join("policy.toml"),
        "[clients.\"mcp-stdio\"]\ndeny = [\"pause\"]\n",
    )
    .expect("policy.toml");
    let mut s = Session::start_in(&dir);
    initialize(&mut s);
    s.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "pause", "arguments": {"zone": "Den"}}
    }));
    let call = s.response(2);
    assert_eq!(call["result"]["isError"], true, "{call}");
    let text = call["result"]["content"][0]["text"].as_str().expect("text");
    assert!(
        text.starts_with("POLICY_DENIED: mcp-stdio may not use pause"),
        "{call}"
    );
}
