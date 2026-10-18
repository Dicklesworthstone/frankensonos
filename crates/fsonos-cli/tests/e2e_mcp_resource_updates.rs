//! MCP resource subscriptions, end to end (8hhv.6): an agent subscribed to
//! `sonos://zones` over the daemon's MCP listener is told when a speaker
//! changes (here, as the Sonos app would: on the player itself), and stops
//! being told once it unsubscribes. The 2024-11-05 SSE session carries the
//! subscription, the notifications and the replies on one stream.

mod e2e;

use e2e::{Daemon, Scenario, http};
use fsonos_proto::control::set_volume;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const ZONES: &str = "sonos://zones";

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// One server-sent event: its kind and its data line(s).
#[derive(Debug, Clone)]
struct Sse {
    kind: String,
    data: String,
}

impl Sse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or(Value::Null)
    }
}

/// A 2024-11-05 MCP session: its `GET /sse` stream, read incrementally, and
/// the path its messages are posted to.
struct Session {
    mcp: String,
    stream: TcpStream,
    raw: String,
    /// Events taken off the stream by [`Self::wait`] so far.
    taken: usize,
    path: String,
}

impl Session {
    /// The session, or what the stream said instead.
    fn open(mcp: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(mcp).map_err(|e| format!("connect: {e}"))?;
        let request =
            format!("GET /sse HTTP/1.1\r\nHost: {mcp}\r\nAccept: text/event-stream\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| format!("timeout: {e}"))?;
        let mut session = Self {
            mcp: mcp.to_owned(),
            stream,
            raw: String::new(),
            taken: 0,
            path: String::new(),
        };
        let Some(endpoint) = session.wait(Duration::from_secs(15), |e| e.kind == "endpoint") else {
            return Err(format!("no endpoint event: {:?}", session.raw));
        };
        endpoint
            .data
            .strip_prefix(&format!("http://{mcp}"))
            .unwrap_or(&endpoint.data)
            .clone_into(&mut session.path);
        Ok(session)
    }

    /// Every complete event on the stream so far.
    fn events(&self) -> Vec<Sse> {
        let Some(body_at) = self.raw.find("\r\n\r\n") else {
            return Vec::new();
        };
        // Chunk-size lines sit between frames; each frame is one chunk.
        let body = self.raw[body_at + 4..].replace("\r\n", "\n");
        let mut events = Vec::new();
        let complete = body.rfind("\n\n").map_or("", |end| &body[..end]);
        for block in complete.split("\n\n") {
            let mut sse = Sse {
                kind: String::new(),
                data: String::new(),
            };
            for line in block.lines() {
                if let Some(kind) = line.strip_prefix("event:") {
                    kind.trim().clone_into(&mut sse.kind);
                } else if let Some(data) = line.strip_prefix("data:") {
                    sse.data.push_str(data.trim());
                }
            }
            if !sse.kind.is_empty() && !sse.data.is_empty() {
                events.push(sse);
            }
        }
        events
    }

    /// The next event not yet taken that matches `want`, waiting up to
    /// `within`; every event before it is taken too.
    fn wait(&mut self, within: Duration, want: impl Fn(&Sse) -> bool) -> Option<Sse> {
        let deadline = Instant::now() + within;
        loop {
            let events = self.events();
            if let Some(at) = events[self.taken..].iter().position(&want) {
                self.taken += at + 1;
                return Some(events[self.taken - 1].clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut buf = [0u8; 8192];
            if let Ok(n) = self.stream.read(&mut buf) {
                self.raw.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }
    }

    /// Wait `within`, then take every event that has arrived.
    fn settle(&mut self, within: Duration) {
        let _ = self.wait(within, |_| false);
        self.taken = self.events().len();
    }

    /// Post a JSON-RPC message to the session; its HTTP status.
    fn post(&self, message: &Value) -> u16 {
        http(
            &self.mcp,
            "POST",
            &self.path,
            &[("Content-Type", "application/json")],
            &message.to_string(),
        )
        .map_or(0, |(code, _, _)| code)
    }

    /// Call `method`, and wait for its reply on the stream.
    fn call(&mut self, id: i64, method: &str, params: &Value) -> Option<Value> {
        let posted =
            self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        if posted != 202 {
            return None;
        }
        self.wait(Duration::from_secs(10), |e| e.json()["id"] == json!(id))
            .map(|e| e.json())
    }
}

/// Whether `event` tells the session `uri` changed.
fn updated(uri: &str) -> impl Fn(&Sse) -> bool + '_ {
    move |e| {
        let message = e.json();
        message["method"] == "notifications/resources/updated" && message["params"]["uri"] == uri
    }
}

/// `fsonos serve` on loopback, up and live: the daemon and its MCP address.
fn serve(s: &mut Scenario) -> (Daemon, String) {
    let mut daemon = s.spawn(
        "serve",
        &[
            "serve",
            "--http",
            "127.0.0.1:0",
            "--mcp-http",
            "127.0.0.1:0",
        ],
    );
    let ready = daemon
        .wait_line("fsonos serve: ready", Duration::from_secs(20))
        .unwrap_or_default();
    let mcp = field(&ready, "mcp")
        .and_then(|u| u.strip_prefix("http://"))
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap_or("")
        .to_owned();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "ready",
        "daemon",
        "serve is up on loopback and found the households",
        mcp.starts_with("127.0.0.1:") && live.is_some(),
        daemon.seen.join("\n"),
    );
    (daemon, mcp)
}

/// A 2024-11-05 MCP session on `mcp`, initialized; `None` if it could not
/// be opened.
fn initialized_session(s: &mut Scenario, mcp: &str) -> Option<Session> {
    let opened = Session::open(mcp);
    s.check(
        "session",
        "mcp-sse",
        "GET /sse opens a 2024-11-05 session and names its message endpoint",
        opened
            .as_ref()
            .is_ok_and(|session| session.path.starts_with("/messages")),
        match &opened {
            Ok(session) => session.raw.clone(),
            Err(why) => why.clone(),
        },
    );
    let mut session = opened.ok()?;
    let initialized = session.call(
        1,
        "initialize",
        &json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "e2e-resource-updates", "version": "1"}
        }),
    );
    let subscribe_advertised = initialized
        .as_ref()
        .is_some_and(|reply| reply["result"]["capabilities"]["resources"]["subscribe"] == true);
    s.check(
        "initialize",
        "mcp-sse",
        "initialize answers, and the server advertises resource subscriptions",
        subscribe_advertised,
        format!("{initialized:?}"),
    );
    let _ = session.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    Some(session)
}

#[test]
fn a_subscribed_agent_hears_a_speaker_change_until_it_unsubscribes() {
    let mut s = Scenario::start("mcp_resource_updates");
    s.sim(SimHousehold::standard());
    let (mut daemon, mcp) = serve(&mut s);
    let Some(mut session) = initialized_session(&mut s, &mcp) else {
        s.finish();
        return;
    };

    let subscribed = session.call(2, "resources/subscribe", &json!({"uri": ZONES}));
    s.check(
        "subscribe",
        "mcp-sse",
        "resources/subscribe to sonos://zones succeeds",
        subscribed
            .as_ref()
            .is_some_and(|reply| reply.get("error").is_none()),
        format!("{subscribed:?}"),
    );

    // A new subscription is told at once (its resources may have changed
    // before it subscribed); take that first, so the change below is what
    // the next notification answers.
    session.settle(Duration::from_secs(3));

    // The Sonos app turns the Kitchen up, on the player itself.
    let (lan, kitchen) = (s.lan(), s.ip("Kitchen"));
    let set = set_volume(&lan, kitchen, 37);
    let heard = session.wait(Duration::from_secs(10), updated(ZONES));
    s.check(
        "updated",
        "mcp-sse",
        "a change on a speaker arrives as notifications/resources/updated for sonos://zones",
        set.is_ok() && heard.is_some(),
        format!("set {set:?}; heard {heard:?}; stream {}", session.raw),
    );

    let unsubscribed = session.call(3, "resources/unsubscribe", &json!({"uri": ZONES}));
    s.check(
        "unsubscribe",
        "mcp-sse",
        "resources/unsubscribe succeeds",
        unsubscribed
            .as_ref()
            .is_some_and(|reply| reply.get("error").is_none()),
        format!("{unsubscribed:?}"),
    );
    // Anything already on its way is taken off first.
    session.settle(Duration::from_secs(2));
    let set = set_volume(&lan, kitchen, 21);
    let after = session.wait(Duration::from_secs(5), updated(ZONES));
    s.check(
        "stopped",
        "mcp-sse",
        "after unsubscribing, a further change sends no notification",
        set.is_ok() && after.is_none(),
        format!("set {set:?}; heard {after:?}"),
    );

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly (exit 0)",
        code == Some(0),
        format!("exit {code:?}"),
    );
    s.finish();
}
