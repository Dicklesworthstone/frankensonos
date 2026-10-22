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
        Self::spawn(data_dir, &[])
    }

    /// Start `fsonos mcp <args>` with the default data directory.
    fn start_with(args: &[&str]) -> Self {
        Self::spawn(&std::env::temp_dir().join("fsonos-mcp-stdio-default"), args)
    }

    fn spawn(data_dir: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fsonos"))
            .arg("mcp")
            .args(args)
            .env_remove("FSONOS_MCP_PROFILE")
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

/// `--profile essentials` offers just the tools most agent tasks need,
/// a far shorter tools/list than the full set (both sizes are printed).
#[test]
fn the_essentials_profile_offers_only_the_essential_tools() {
    let list = |args: &[&str]| {
        let mut s = Session::start_with(args);
        initialize(&mut s);
        s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        s.response(2)
    };
    let names = |list: &Value| -> Vec<String> {
        let mut names: Vec<String> = list["result"]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    };
    let full = list(&[]);
    let essentials = list(&["--profile", "essentials"]);
    let mut want: Vec<String> = fsonos_mcp::ESSENTIALS
        .iter()
        .map(|n| (*n).to_owned())
        .collect();
    want.sort();
    assert_eq!(names(&essentials), want, "{essentials}");
    let every = names(&full);
    assert!(want.iter().all(|n| every.contains(n)), "{every:?}");
    let bytes = |list: &Value| list["result"].to_string().len();
    let (all, few) = (bytes(&full), bytes(&essentials));
    eprintln!(
        "tools/list: full {} tools, {all} bytes (~{} tokens); essentials {} tools, {few} bytes \
         (~{} tokens)",
        every.len(),
        all / 4,
        want.len(),
        few / 4
    );
    assert!(few * 3 < all, "essentials {few} bytes, full {all}");
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
        "seek",
        "set_volume",
        "mute",
        "group",
        "ungroup",
        "dj_start",
        "dj_skip",
        "dj_stop",
        "dj_steer",
        "dj_status",
        "dj_moods",
        "dj_rehearse",
        "search_queue",
        "announce",
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
    let steer = &tool("dj_steer")["inputSchema"];
    for arg in [
        "mood",
        "include_composers",
        "energy_bias",
        "for_minutes",
        "clear",
    ] {
        assert!(
            steer["properties"].get(arg).is_some(),
            "dj_steer lacks {arg}: {steer}"
        );
    }
    assert_eq!(steer["required"], json!(["zone"]), "{steer}");
    let start = &tool("dj_start")["inputSchema"];
    for (name, schema) in steer["properties"].as_object().unwrap() {
        if name != "clear" {
            assert_eq!(
                &start["properties"][name], schema,
                "dj_start must expose the same {name} argument as dj_steer"
            );
        }
    }
    assert!(start["properties"].get("clear").is_none());
    assert_eq!(start["required"], json!(["zone"]), "{start}");
    let announcement = &tool("announce")["inputSchema"];
    for arg in ["text", "chime", "wav_base64", "voice", "rooms", "volume"] {
        assert!(
            announcement["properties"].get(arg).is_some(),
            "announce lacks {arg}: {announcement}"
        );
    }
    assert!(announcement["properties"].get("file").is_none());
    assert_eq!(
        tool("dj_status")["annotations"]["readOnlyHint"],
        true,
        "{list}"
    );
    let rehearse = tool("dj_rehearse");
    assert_eq!(rehearse["annotations"]["readOnlyHint"], true, "{rehearse}");
    for arg in ["zone", "works", "hours", "at", "mood", "seed"] {
        assert!(
            rehearse["inputSchema"]["properties"].get(arg).is_some(),
            "dj_rehearse lacks {arg}: {rehearse}"
        );
    }
    assert_eq!(rehearse["inputSchema"]["required"], json!(["zone"]));
    let queue_search = tool("search_queue");
    assert_eq!(queue_search["annotations"]["readOnlyHint"], true);
    assert_eq!(queue_search["annotations"]["idempotentHint"], true);
    for arg in ["zone", "query", "offset", "limit", "expected_revision"] {
        assert!(
            queue_search["inputSchema"]["properties"].get(arg).is_some(),
            "search_queue lacks {arg}: {queue_search}"
        );
    }
    assert_eq!(
        queue_search["inputSchema"]["required"],
        json!(["zone", "query"])
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
