//! The e2e harness: drive the real `fsonos` binary against `fsonos-sim`, and
//! log every step as JSON lines.
//!
//! A scenario is one `#[test]`:
//!
//! ```ignore
//! mod e2e;
//! use e2e::Scenario;
//!
//! #[test]
//! fn discover_finds_both_households() {
//!     let mut s = Scenario::start("discover");
//!     s.sim(fsonos_sim::SimHousehold::standard());
//!     let run = s.cli("discover", &["discover", "--json"]);
//!     s.check("discover", "cli", "exits 0", run.code == Some(0), &run.stderr);
//!     s.pending("zones", "needs the routed LAN transport");
//!     s.finish(); // prints the summary; panics if any check failed
//! }
//! ```
//!
//! What every scenario gets:
//!
//! * Isolation from the real house: a temp `FSONOS_DATA_DIR`, the HTTP and
//!   MCP listeners pinned to ephemeral loopback ports, `FSONOS_SEEDS`
//!   pointing at the sim's seeds file, `FSONOS_ROUTES` sending each player's
//!   advertised address to its loopback socket and SSDP to the sim's unicast
//!   responder (no multicast), and no Spotify settings. Tripwire
//!   listeners sit on the default ports (8099, 8098); a connection to either
//!   fails the scenario.
//! * Logs in `target/e2e-logs/<scenario>/<epoch-ms>/` (or under
//!   `FSONOS_E2E_LOG_DIR`): `steps.jsonl` with one line per step
//!   (`ts, scenario, step, surface, command, exit_code, stdout, stderr,
//!   duration_ms, assertion, status`), the full stdout/stderr of each run in
//!   side files, `soap.log` / `gena.log` from the sim, and `summary.json`.
//! * [`Scenario::pending`] for steps whose feature has not landed: counted
//!   and shown, never reported as a pass.
//!
//! No bare sleeps: anything that waits polls with a deadline.

#![allow(dead_code)] // each test binary uses a different subset

use fsonos_sim::{SimBuilder, SimHandle, SimLan};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Longest stdout/stderr kept inline in `steps.jsonl`.
const INLINE: usize = 2000;

/// The settings the harness controls; any ambient value is removed first.
const SETTINGS: [&str; 8] = [
    "FSONOS_ROUTES",
    "FSONOS_HTTP_ADDR",
    "FSONOS_MCP_HTTP_ADDR",
    "FSONOS_DATA_DIR",
    "FSONOS_SEEDS",
    "FSONOS_SPOTIFY_CLIENT_ID",
    "FSONOS_SPOTIFY_REDIRECT_URI",
    "RUST_LOG",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Pending,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Pending => "pending",
        }
    }
}

/// One run of the `fsonos` binary.
#[derive(Debug, Clone)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
}

impl Run {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// A running scenario. See the module docs.
pub struct Scenario {
    name: String,
    dir: PathBuf,
    log: File,
    seq: usize,
    tally: Vec<(String, Status)>,
    sim: Option<SimHandle>,
}

/// The tripwire listeners on the default ports, bound once per test process
/// and shared by its scenarios (the tests in one binary run in parallel). A
/// port another process already holds has no tripwire.
fn tripwires() -> &'static [(u16, TcpListener)] {
    static TRIPWIRES: OnceLock<Vec<(u16, TcpListener)>> = OnceLock::new();
    TRIPWIRES.get_or_init(|| {
        [8099, 8098]
            .into_iter()
            .filter_map(|port| {
                let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
                listener.set_nonblocking(true).ok()?;
                Some((port, listener))
            })
            .collect()
    })
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn truncated(text: &str) -> String {
    if text.len() <= INLINE {
        return text.to_string();
    }
    let mut end = INLINE;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes; see side file)", &text[..end], text.len())
}

impl Scenario {
    /// Start a scenario: its log directory, data directory and tripwires.
    #[must_use]
    pub fn start(name: &str) -> Self {
        let root = std::env::var_os("FSONOS_E2E_LOG_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/e2e-logs"),
            PathBuf::from,
        );
        let dir = root.join(name).join(epoch_ms().to_string());
        fs::create_dir_all(dir.join("data")).expect("create the scenario log dir");
        let log = File::create(dir.join("steps.jsonl")).expect("create steps.jsonl");
        let mut s = Self {
            name: name.to_string(),
            dir,
            log,
            seq: 0,
            tally: Vec::new(),
            sim: None,
        };
        for port in [8099, 8098] {
            if !tripwires().iter().any(|(p, _)| *p == port) {
                s.pending(
                    &format!("tripwire-{port}"),
                    &format!(
                        "127.0.0.1:{port} is in use, so leaks to that default port can't be detected"
                    ),
                );
            }
        }
        s
    }

    /// The scenario's log directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Spawn the virtual households and write their seeds file.
    pub fn sim(&mut self, builder: SimBuilder) -> &SimHandle {
        let started = Instant::now();
        let sim = builder.spawn().expect("spawn fsonos-sim");
        fs::write(self.dir.join("seeds.toml"), sim.seeds_toml()).expect("write seeds.toml");
        fs::write(self.dir.join("routes.toml"), routes_toml(&sim)).expect("write routes.toml");
        let detail = format!("{} players", sim.players().len());
        self.record(
            "sim",
            "sim",
            "spawn",
            None,
            "",
            "",
            started.elapsed().as_millis(),
            "the virtual households are up",
            Status::Pass,
            &detail,
        );
        self.sim.insert(sim)
    }

    /// A transport that reads the sim's state directly (by advertised IP).
    #[must_use]
    pub fn lan(&self) -> SimLan {
        self.sim.as_ref().expect("Scenario::sim first").lan()
    }

    /// The advertised address of the virtual player in `room`.
    #[must_use]
    pub fn ip(&self, room: &str) -> std::net::IpAddr {
        self.sim
            .as_ref()
            .and_then(|s| s.player(room))
            .unwrap_or_else(|| panic!("no virtual player in {room}"))
            .ip
    }

    /// The running sim, if [`Self::sim`] spawned one.
    #[must_use]
    pub fn sim_handle(&self) -> Option<&SimHandle> {
        self.sim.as_ref()
    }

    /// `fsonos` with the isolation environment applied.
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fsonos"));
        for var in SETTINGS {
            cmd.env_remove(var);
        }
        cmd.env("FSONOS_DATA_DIR", self.dir.join("data"))
            .env("FSONOS_HTTP_ADDR", "127.0.0.1:0")
            .env("FSONOS_MCP_HTTP_ADDR", "127.0.0.1:0")
            .env("RUST_LOG", "warn");
        if self.sim.is_some() {
            cmd.env("FSONOS_SEEDS", self.dir.join("seeds.toml"))
                .env("FSONOS_ROUTES", self.dir.join("routes.toml"));
        }
        cmd
    }

    /// Run `fsonos <args>` to completion and log it as step `step`. The
    /// caller asserts on the result with [`Self::check`].
    pub fn cli(&mut self, step: &str, args: &[&str]) -> Run {
        let started = Instant::now();
        let out = self
            .command()
            .args(args)
            .output()
            .expect("run the fsonos binary");
        let run = Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            duration_ms: started.elapsed().as_millis(),
        };
        let command = format!("fsonos {}", args.join(" "));
        self.record(
            step,
            "cli",
            &command,
            run.code,
            &run.stdout,
            &run.stderr,
            run.duration_ms,
            "ran",
            Status::Pass,
            "",
        );
        run
    }

    /// Start `fsonos mcp` (stdio) with the isolation environment.
    pub fn mcp(&mut self) -> McpSession {
        let mut cmd = self.command();
        cmd.arg("mcp");
        let session = McpSession::spawn(cmd);
        self.record(
            "mcp-spawn",
            "mcp",
            "fsonos mcp",
            None,
            "",
            "",
            0,
            "spawned",
            Status::Pass,
            "",
        );
        session
    }

    /// Record an assertion about `step` on `surface`.
    pub fn check(
        &mut self,
        step: &str,
        surface: &str,
        assertion: &str,
        pass: bool,
        detail: impl std::fmt::Display,
    ) {
        let status = if pass { Status::Pass } else { Status::Fail };
        self.record(
            step,
            surface,
            "",
            None,
            "",
            "",
            0,
            assertion,
            status,
            &detail.to_string(),
        );
    }

    /// Mark `step` as waiting on a feature that has not landed.
    pub fn pending(&mut self, step: &str, reason: &str) {
        self.record(step, "", "", None, "", "", 0, reason, Status::Pending, "");
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        step: &str,
        surface: &str,
        command: &str,
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
        duration_ms: u128,
        assertion: &str,
        status: Status,
        detail: &str,
    ) {
        self.seq += 1;
        if stdout.len() > INLINE || stderr.len() > INLINE {
            let side = format!("{:02}-{step}", self.seq);
            fs::write(self.dir.join(format!("{side}.stdout")), stdout).ok();
            fs::write(self.dir.join(format!("{side}.stderr")), stderr).ok();
        }
        let line = json!({
            "ts": epoch_ms(),
            "scenario": self.name,
            "step": step,
            "surface": surface,
            "command": command,
            "exit_code": exit_code,
            "stdout": truncated(stdout),
            "stderr": truncated(stderr),
            "duration_ms": duration_ms,
            "assertion": assertion,
            "detail": detail,
            "status": status.as_str(),
            "pass": status == Status::Pass,
        });
        writeln!(self.log, "{line}").expect("append to steps.jsonl");
        if status != Status::Pass || (!assertion.is_empty() && assertion != "ran") {
            let mut what = format!("{step}: {assertion}");
            if status == Status::Fail && !detail.is_empty() {
                what.push_str(" -- ");
                what.push_str(&truncated(detail));
            }
            self.tally.push((what, status));
        }
    }

    /// Check the tripwires, save the sim's logs and the summary, print it,
    /// and panic if any check failed.
    pub fn finish(mut self) -> Summary {
        let tripped: Vec<u16> = tripwires()
            .iter()
            .filter(|(_, l)| l.accept().is_ok())
            .map(|(port, _)| *port)
            .collect();
        for port in [8099, 8098] {
            if tripwires().iter().any(|(p, _)| *p == port) {
                let pass = !tripped.contains(&port);
                self.check(
                    &format!("tripwire-{port}"),
                    "isolation",
                    &format!("nothing connected to the default port {port}"),
                    pass,
                    "",
                );
            }
        }
        let mut soap_tail = Vec::new();
        if let Some(sim) = &self.sim {
            let soap: Vec<String> = sim.soap_log().iter().map(|e| format!("{e:?}")).collect();
            let gena: Vec<String> = sim.gena_log().iter().map(|e| format!("{e:?}")).collect();
            fs::write(self.dir.join("soap.log"), soap.join("\n")).ok();
            fs::write(self.dir.join("gena.log"), gena.join("\n")).ok();
            soap_tail = soap.iter().rev().take(50).rev().cloned().collect();
        }
        let count = |s: Status| self.tally.iter().filter(|(_, t)| *t == s).count();
        let summary = Summary {
            passed: count(Status::Pass),
            failed: count(Status::Fail),
            pending: count(Status::Pending),
            dir: self.dir.clone(),
        };
        let lines: Vec<Value> = self
            .tally
            .iter()
            .map(|(what, s)| json!({ "check": what, "status": s.as_str() }))
            .collect();
        fs::write(
            self.dir.join("summary.json"),
            json!({
                "scenario": self.name,
                "passed": summary.passed,
                "failed": summary.failed,
                "pending": summary.pending,
                "checks": lines,
            })
            .to_string(),
        )
        .ok();
        println!(
            "e2e {}: {} passed, {} failed, {} pending; logs: {}",
            self.name,
            summary.passed,
            summary.failed,
            summary.pending,
            self.dir.display()
        );
        for (what, status) in &self.tally {
            if *status != Status::Pass {
                println!("  {}: {what}", status.as_str());
            }
        }
        if summary.failed > 0 {
            if !soap_tail.is_empty() {
                println!("last {} sim SOAP exchanges:", soap_tail.len());
                for entry in &soap_tail {
                    println!("  {entry}");
                }
            }
            panic!(
                "e2e {}: {} check(s) failed; see {}",
                self.name,
                summary.failed,
                self.dir.display()
            );
        }
        summary
    }
}

/// The `--routes` file for `sim`: SSDP to its unicast responder, and each
/// player's advertised address to its loopback socket.
fn routes_toml(sim: &SimHandle) -> String {
    let routes: Vec<String> = sim
        .players()
        .iter()
        .map(|p| format!("\"{}\" = \"{}\"", p.ip, p.addr))
        .collect();
    format!(
        "ssdp = \"{}\"\n\n[routes]\n{}\n",
        sim.ssdp_addr(),
        routes.join("\n")
    )
}

impl Scenario {
    /// Start a long-running `fsonos <args>` (a daemon) with the isolation
    /// environment, logged as step `step`.
    pub fn spawn(&mut self, step: &str, args: &[&str]) -> Daemon {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn fsonos");
        let stderr = child.stderr.take().expect("daemon stderr");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let command = format!("fsonos {}", args.join(" "));
        self.record(
            step,
            "daemon",
            &command,
            None,
            "",
            "",
            0,
            "spawned",
            Status::Pass,
            "",
        );
        Daemon {
            child,
            lines,
            seen: Vec::new(),
        }
    }
}

/// A long-running `fsonos` process started by [`Scenario::spawn`].
pub struct Daemon {
    child: Child,
    lines: Receiver<String>,
    /// Every stderr line read so far.
    pub seen: Vec<String>,
}

impl Daemon {
    /// The first stderr line containing `needle`, waiting up to `timeout`.
    pub fn wait_line(&mut self, needle: &str, timeout: Duration) -> Option<String> {
        if let Some(line) = self.seen.iter().find(|l| l.contains(needle)) {
            return Some(line.clone());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).ok()?;
            self.seen.push(line.clone());
            if line.contains(needle) {
                return Some(line);
            }
        }
    }

    /// Send SIGINT and wait up to `timeout` for the exit code.
    pub fn interrupt(&mut self, timeout: Duration) -> Option<i32> {
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(50)) {
                    self.seen.push(line);
                }
                return status.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `(status, lowercased headers, body)` from [`http`].
pub type HttpAnswer = (u16, Vec<(String, String)>, String);

/// A plain HTTP/1.1 exchange over a fresh connection (`Connection: close`).
pub fn http(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> std::io::Result<HttpAnswer> {
    use std::io::Read as _;
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_string()
    };
    Ok((status, headers, body))
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

/// The outcome of [`Scenario::finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    pub dir: PathBuf,
}

/// An MCP session with `fsonos mcp` over stdio.
pub struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl McpSession {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fsonos mcp");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("mcp stdout");
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
            next_id: 1,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("mcp stdin open");
        writeln!(stdin, "{msg}").expect("write to fsonos mcp");
        stdin.flush().expect("flush fsonos mcp");
    }

    /// Send a request and wait (up to 20 s) for its response.
    pub fn request(&mut self, method: &str, params: &Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("fsonos mcp answered {method} within 20s"));
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                panic!("non-JSON on MCP stdout: {line}");
            };
            if msg["id"] == id {
                return msg;
            }
        }
    }

    /// `initialize` (protocol 2024-11-05) plus `notifications/initialized`.
    pub fn initialize(&mut self) -> Value {
        let init = self.request(
            "initialize",
            &json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "fsonos-e2e", "version": "0"}
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
