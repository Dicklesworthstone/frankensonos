//! `GET /events` end to end: a change made on a virtual player directly
//! arrives as a `zone.state` event within a second, a client that
//! reconnects with `Last-Event-ID` gets what it missed, and a command shows
//! up as `action.logged`.

mod e2e;

use e2e::{Scenario, http};
use fsonos_proto::control::set_volume;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// `key=value` out of a daemon line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// One server-sent event as read off the wire.
#[derive(Debug, Clone)]
struct Sse {
    id: Option<u64>,
    kind: String,
    data: Value,
}

/// A `GET /events` connection, read incrementally.
struct Events {
    stream: TcpStream,
    raw: String,
    seen: Vec<Sse>,
}

impl Events {
    fn open(api: &str, last_event_id: Option<u64>) -> std::io::Result<(u16, Self)> {
        let mut stream = TcpStream::connect(api)?;
        let mut request =
            format!("GET /events HTTP/1.1\r\nHost: {api}\r\nAccept: text/event-stream\r\n");
        if let Some(id) = last_event_id {
            let _ = write!(request, "Last-Event-ID: {id}\r\n");
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes())?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut events = Self {
            stream,
            raw: String::new(),
            seen: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !events.raw.contains("\r\n\r\n") && Instant::now() < deadline {
            events.pull();
        }
        let status = events
            .raw
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        Ok((status, events))
    }

    /// Read what has arrived and parse any complete events.
    fn pull(&mut self) {
        let mut buf = [0u8; 8192];
        if let Ok(n) = self.stream.read(&mut buf) {
            self.raw.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        let Some(body_at) = self.raw.find("\r\n\r\n") else {
            return;
        };
        // Chunk-size lines sit between frames; each frame is one chunk.
        let body = &self.raw[body_at + 4..];
        let mut seen = Vec::new();
        for block in body.split("\n\n") {
            let mut sse = Sse {
                id: None,
                kind: String::new(),
                data: Value::Null,
            };
            for line in block.lines() {
                if let Some(id) = line.strip_prefix("id: ") {
                    sse.id = id.trim().parse().ok();
                } else if let Some(kind) = line.strip_prefix("event: ") {
                    sse.kind = kind.trim().to_string();
                } else if let Some(data) = line.strip_prefix("data: ") {
                    sse.data = serde_json::from_str(data.trim()).unwrap_or(Value::Null);
                }
            }
            if !sse.kind.is_empty() && !sse.data.is_null() {
                seen.push(sse);
            }
        }
        self.seen = seen;
    }

    /// The first event matching `want`, waiting up to `within`.
    fn wait(&mut self, within: Duration, want: impl Fn(&Sse) -> bool) -> Option<Sse> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(hit) = self.seen.iter().find(|e| want(e)) {
                return Some(hit.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.pull();
        }
    }
}

fn kitchen_volume(level: u8) -> impl Fn(&Sse) -> bool {
    move |e| {
        e.kind == "zone.state" && e.data["room"] == "Kitchen" && e.data["volume"] == json!(level)
    }
}

#[test]
fn events_stream_the_house_and_resume() {
    let mut s = Scenario::start("events");
    s.sim(SimHousehold::standard());
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
    let api = field(&ready, "http")
        .and_then(|u| u.strip_prefix("http://"))
        .unwrap_or("")
        .to_string();
    let live = daemon.wait_line("fsonos serve: live", Duration::from_secs(20));
    s.check(
        "live",
        "daemon",
        "the live model is up",
        live.is_some(),
        daemon.seen.join("\n"),
    );

    let opened = Events::open(&api, None);
    let ok = matches!(&opened, Ok((200, e)) if e.raw.to_ascii_lowercase().contains("content-type: text/event-stream"));
    s.check(
        "subscribe",
        "http",
        "GET /events answers 200 text/event-stream",
        ok,
        opened
            .as_ref()
            .map_or_else(ToString::to_string, |(_, ev)| ev.raw.clone()),
    );
    let Ok((_, mut events)) = opened else {
        s.finish();
        return;
    };

    // A change on the player itself arrives within a second.
    let kitchen = s.ip("Kitchen");
    let lan = s.lan();
    let changed_at = Instant::now();
    let set = set_volume(&lan, kitchen, 41);
    let first = events.wait(Duration::from_secs(1), kitchen_volume(41));
    s.check(
        "zone-state",
        "sse",
        "a volume set on the Kitchen player arrives as zone.state within 1 s",
        set.is_ok() && first.is_some(),
        format!(
            "{:?} after {:?}; seen {:?}",
            first,
            changed_at.elapsed(),
            events.seen
        ),
    );
    let first_id = first.and_then(|e| e.id).unwrap_or(0);

    drop(events);
    check_resume(&mut s, &api, first_id);
    check_action_logged(&mut s, &api);

    let code = daemon.interrupt(Duration::from_secs(10));
    s.check(
        "stop",
        "daemon",
        "SIGINT stops serve cleanly",
        code == Some(0),
        format!("{code:?}"),
    );
    s.finish();
}

/// Disconnected, the client misses two changes; resuming after `first_id`
/// replays them, in order.
fn check_resume(s: &mut Scenario, api: &str, first_id: u64) {
    let (kitchen, lan) = (s.ip("Kitchen"), s.lan());
    let _ = set_volume(&lan, kitchen, 42);
    let _ = set_volume(&lan, kitchen, 43);
    std::thread::sleep(Duration::from_millis(500));
    let resumed = Events::open(api, Some(first_id));
    let replayed = resumed.ok().and_then(|(_, mut ev)| {
        let a = ev.wait(Duration::from_secs(2), kitchen_volume(42))?;
        let b = ev.wait(Duration::from_secs(2), kitchen_volume(43))?;
        Some((a.id?, b.id?))
    });
    s.check(
        "resume",
        "sse",
        "reconnecting with Last-Event-ID replays the missed changes, in order",
        replayed.is_some_and(|(a, b)| first_id < a && a < b),
        format!("after {first_id}: {replayed:?}"),
    );
}

/// A command is logged, and the log entry is an event too.
fn check_action_logged(s: &mut Scenario, api: &str) {
    let (_, mut events) = match Events::open(api, None) {
        Ok(opened) => opened,
        Err(e) => panic!("reopen /events: {e}"),
    };
    let set = http(
        api,
        "POST",
        "/volume",
        &[("Content-Type", "application/json")],
        &json!({ "zone": "Kitchen", "volume": 25 }).to_string(),
    );
    let logged = events.wait(Duration::from_secs(2), |e| {
        e.kind == "action.logged"
            && e.data["intent"]
                .as_str()
                .is_some_and(|i| i.starts_with("set_volume"))
    });
    s.check(
        "action-logged",
        "sse",
        "POST /volume shows up as action.logged",
        set.as_ref().is_ok_and(|(code, _, _)| *code == 200) && logged.is_some(),
        format!("{set:?}; {logged:?}"),
    );
}
