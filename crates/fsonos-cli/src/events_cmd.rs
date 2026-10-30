//! `fsonos events`: the house's changes as they happen, from a running
//! daemon's event stream (`GET /events`, see `fsonos_api::events`). A line
//! per change in words (a room's volume or play state, a player going
//! offline, a logged action, the DJ's picks), or with `--json` an object per
//! line (`{id, event, data}`) for a script. `--since <id>` first replays what
//! came after that event; `--count` stops after that many. Daemon URLs, including
//! Tailscale Serve HTTPS, use cursor-preserving long polls through the HTTP client.

use clap::Args;
use fsonos_api::events::HEARTBEAT;
use fsonos_api::{ActionDto, DjStatusDto, DjWorkDto, ErrorCode, Failure};
use fsonos_core::clock::{Clock, SystemClock};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};

use crate::config::GlobalArgs;
use crate::direct;
use crate::remote::Daemon;

mod polling;

#[derive(Debug, Args)]
pub struct EventsArgs {
    /// First replay the events after this one (an `id` from `--json`); by
    /// default only what happens from now on.
    #[arg(long, value_name = "ID")]
    pub since: Option<u64>,
    /// Stop after this many events.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub count: Option<u64>,
}

/// `fsonos events`: print the daemon's events until `--count`, the reader
/// going away, or the stream ending.
pub fn run(global: &GlobalArgs, args: &EventsArgs) -> anyhow::Result<()> {
    let Some(daemon) = Daemon::find(global, true)? else {
        return Err(Failure::new(
            ErrorCode::NotReady,
            "fsonos events streams from the daemon, and --direct never asks it",
        )
        .with_hint("Drop --direct, with fsonos serve running.")
        .into());
    };
    let mut output = Output {
        writer: io::stdout().lock(),
        json: global.json,
        limit: args.count,
        shown: 0,
    };
    if daemon.uses_url() {
        return polling::run(
            move |since, wait| daemon.events_after(since, wait),
            args.since,
            |event| output.show(&event),
        );
    }
    let path = args
        .since
        .map_or_else(|| "/events".to_owned(), |id| format!("/events?since={id}"));
    // Three heartbeats missed: the daemon is gone, not the house quiet.
    let quiet = HEARTBEAT * 3;
    let (reader, chunked) = daemon.open_stream(&path, quiet)?;
    let mut body: Box<dyn BufRead> = if chunked {
        Box::new(BufReader::new(Chunked::new(reader)))
    } else {
        Box::new(reader)
    };
    let mut parser = Parser::default();
    let mut last = args.since;
    let mut line = String::new();
    loop {
        line.clear();
        match body.read_line(&mut line) {
            Ok(0) => return Err(ended(None, quiet, last).into()),
            Ok(_) => {}
            Err(e) => return Err(ended(Some(&e), quiet, last).into()),
        }
        let Some(event) = parser.line(line.trim_end_matches(['\n', '\r'])) else {
            continue;
        };
        last = event.id.or(last);
        if !output.show(&event)? {
            return Ok(());
        }
    }
}

/// SSE and URL polling print the same lines and count reset notices alike.
struct Output<W> {
    writer: W,
    json: bool,
    limit: Option<u64>,
    shown: u64,
}

impl<W: Write> Output<W> {
    fn show(&mut self, event: &Event) -> io::Result<bool> {
        let text = if self.json {
            format!("{}\n", event.json())
        } else {
            event_text(event, &SystemClock.now().format("%H:%M:%S").to_string())
        };
        if let Err(error) = self
            .writer
            .write_all(text.as_bytes())
            .and_then(|()| self.writer.flush())
        {
            // A pipe reader such as head has left once it has enough events.
            return if error.kind() == io::ErrorKind::BrokenPipe {
                Ok(false)
            } else {
                Err(error)
            };
        }
        self.shown += 1;
        Ok(self.limit.is_none_or(|limit| self.shown < limit))
    }
}

/// The stream ended (`why` is `None` when the daemon closed it): what to
/// say, and how to pick up after `last`, the last event shown.
fn ended(why: Option<&io::Error>, quiet: std::time::Duration, last: Option<u64>) -> Failure {
    let detail = match why {
        None => "the daemon closed the event stream".to_owned(),
        Some(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            format!(
                "the daemon's event stream went quiet: no event or heartbeat in {} s",
                quiet.as_secs()
            )
        }
        Some(e) => format!("the daemon's event stream broke: {e}"),
    };
    let hint = last.map_or_else(
        || "Check that fsonos serve runs, then run fsonos events again.".to_owned(),
        |id| format!("Check that fsonos serve runs; `fsonos events --since {id}` picks up after the last event."),
    );
    Failure::new(ErrorCode::NotReady, detail).with_hint(hint)
}

/// The body of a chunked HTTP response, read as one stream.
struct Chunked<R> {
    inner: R,
    /// What is left of the chunk being read.
    left: usize,
    done: bool,
}

impl<R> Chunked<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            left: 0,
            done: false,
        }
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // The next size line, past the CRLF that ends the chunk before.
            let mut size = String::new();
            while size.trim().is_empty() {
                size.clear();
                if self.inner.read_line(&mut size)? == 0 {
                    self.done = true;
                    return Ok(0);
                }
            }
            let hex = size.split(';').next().unwrap_or_default().trim();
            self.left = usize::from_str_radix(hex, 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad chunk size {hex:?}"),
                )
            })?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n;
        Ok(n)
    }
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq)]
struct Event {
    id: Option<u64>,
    kind: String,
    data: Value,
}

impl Event {
    /// `--json`: `{id, event, data}`, on one line.
    fn json(&self) -> Value {
        json!({ "id": self.id, "event": self.kind, "data": self.data })
    }
}

/// Server-sent events out of their lines: fields gather until a blank line
/// ends the event; comments (the heartbeat) are skipped.
#[derive(Debug, Default)]
struct Parser {
    id: Option<u64>,
    kind: Option<String>,
    data: Vec<String>,
}

impl Parser {
    /// Take one line (its newline gone): the event it completes, if any.
    fn line(&mut self, line: &str) -> Option<Event> {
        if line.is_empty() {
            let (id, kind, data) = (
                self.id.take(),
                self.kind.take(),
                std::mem::take(&mut self.data),
            );
            if kind.is_none() && data.is_empty() {
                return None;
            }
            let data = data.join("\n");
            return Some(Event {
                id,
                kind: kind.unwrap_or_else(|| "message".to_owned()),
                data: serde_json::from_str(&data).unwrap_or(Value::String(data)),
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => self.id = value.parse().ok(),
            "event" => self.kind = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }
}

/// `event` in words, after the local time `at`, as one line.
fn event_text(event: &Event, at: &str) -> String {
    let d = &event.data;
    let field = |k: &str| d.get(k).and_then(Value::as_str);
    let room = field("room")
        .or_else(|| field("player"))
        .unwrap_or("a player");
    let what = match event.kind.as_str() {
        "zone.state" => format!("{room}: {}", changes(d)),
        "player.health" => format!("{room}: {}", field("health").unwrap_or("health unknown")),
        "topology.changed" => format!(
            "the rooms or groups changed: {} rooms in {} households now",
            d["rooms"], d["households"]
        ),
        "action.logged" => logged(d),
        "dj.status" => dj(d),
        "events.reset" => format!(
            "events before #{} are gone: `fsonos zones` reads the house afresh",
            d["oldest"]
        ),
        other => format!("{other}: {d}"),
    };
    format!("{at} {what}\n")
}

/// What a `zone.state` event says changed.
fn changes(d: &Value) -> String {
    let mut said = Vec::new();
    if let Some(transport) = d.get("transport").and_then(Value::as_str) {
        said.push(transport.to_owned());
    }
    if d.get("source_changed").and_then(Value::as_bool) == Some(true) {
        said.push("source changed".to_owned());
    }
    // The track's URI now: it moved on, or (null) the queue ran out.
    match d.get("track") {
        Some(Value::String(_)) => said.push("a new track".to_owned()),
        Some(Value::Null) => said.push("the queue ended".to_owned()),
        _ => {}
    }
    if let Some(volume) = d.get("volume").and_then(Value::as_u64) {
        said.push(format!("volume {volume}"));
    }
    if let Some(mute) = d.get("mute").and_then(Value::as_bool) {
        said.push(if mute { "muted" } else { "unmuted" }.to_owned());
    }
    if let Some(volume) = d.get("group_volume").and_then(Value::as_u64) {
        said.push(format!("group volume {volume}"));
    }
    if let Some(length) = d.get("queue_length").and_then(Value::as_u64) {
        said.push(format!("{length} in the queue"));
    }
    if said.is_empty() {
        "changed".to_owned()
    } else {
        said.join(", ")
    }
}

/// An `action.logged` event as `fsonos log` shows the action.
fn logged(d: &Value) -> String {
    serde_json::from_value::<ActionDto>(d.clone()).map_or_else(
        |_| format!("action.logged: {d}"),
        |a| {
            let line = direct::actions_text(std::slice::from_ref(&a));
            format!("logged {}", line.trim_end())
        },
    )
}

/// A `dj.status` event: what the DJ plays in a zone, and what comes next.
fn dj(d: &Value) -> String {
    let Ok(status) = serde_json::from_value::<DjStatusDto>(d.clone()) else {
        return format!("dj.status: {d}");
    };
    if !status.running {
        return format!("{}: the DJ is off", status.zone);
    }
    let work = |w: &DjWorkDto| format!("{}: {}", w.composer, w.title);
    let mut line = status.now.as_ref().map_or_else(
        || format!("{}: the DJ is on", status.zone),
        |now| format!("{}: the DJ plays {}", status.zone, work(now)),
    );
    if let Some(next) = status.next.first() {
        let _ = write!(line, "; next {}", work(next));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_transports_share_json_output_count_and_broken_pipe_exit() {
        let event = Event {
            id: Some(8),
            kind: "zone.state".into(),
            data: json!({ "room": "Kitchen", "volume": 41 }),
        };
        let mut output = Output {
            writer: Vec::new(),
            json: true,
            limit: Some(1),
            shown: 0,
        };
        assert!(!output.show(&event).unwrap());
        assert_eq!(output.shown, 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.writer).unwrap(),
            event.json()
        );

        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Output {
            writer: Closed,
            json: true,
            limit: None,
            shown: 0,
        };
        assert!(!output.show(&event).unwrap());
        assert_eq!(output.shown, 0);
    }

    /// `body` as one chunk per piece, then the last (empty) chunk.
    fn chunked(pieces: &[&str]) -> Vec<u8> {
        let mut out = String::new();
        for piece in pieces {
            let _ = write!(out, "{:x}\r\n{piece}\r\n", piece.len());
        }
        out.push_str("0\r\n\r\n");
        out.into_bytes()
    }

    fn events_of(raw: &[u8]) -> Vec<Event> {
        let mut body = BufReader::new(Chunked::new(raw));
        let mut parser = Parser::default();
        let mut seen = Vec::new();
        let mut line = String::new();
        while body.read_line(&mut line).unwrap() > 0 {
            seen.extend(parser.line(line.trim_end_matches(['\n', '\r'])));
            line.clear();
        }
        seen
    }

    #[test]
    fn a_chunked_stream_reads_as_its_events_and_skips_heartbeats() {
        let raw = chunked(&[
            ": heartbeat\n\n",
            "id: 7\nevent: zone.state\ndata: {\"room\":\"Kitchen\",\"volume\":41}\n\n",
            // An event split across chunks, its data over two lines.
            "id: 8\nevent: note\ndata: one",
            "\ndata: two\n\n",
            ": heartbeat\n\n",
        ]);
        let seen = events_of(&raw);
        assert_eq!(
            seen,
            [
                Event {
                    id: Some(7),
                    kind: "zone.state".into(),
                    data: json!({ "room": "Kitchen", "volume": 41 }),
                },
                Event {
                    id: Some(8),
                    kind: "note".into(),
                    data: json!("one\ntwo"),
                },
            ]
        );
        assert_eq!(
            seen[0].json(),
            json!({ "id": 7, "event": "zone.state", "data": { "room": "Kitchen", "volume": 41 } })
        );
        let bad = b"zz\r\nx\r\n";
        let mut body = BufReader::new(Chunked::new(&bad[..]));
        let err = body.read_line(&mut String::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn events_read_as_a_line_in_words() {
        let text = |kind: &str, data: Value| {
            event_text(
                &Event {
                    id: Some(1),
                    kind: kind.into(),
                    data,
                },
                "21:04:05",
            )
        };
        assert_eq!(
            text(
                "zone.state",
                json!({ "player": "RINCON_X", "room": "Kitchen", "transport": "playing",
                        "track": "x-sonos-spotify:t", "volume": 41, "mute": true })
            ),
            "21:04:05 Kitchen: playing, a new track, volume 41, muted\n"
        );
        assert_eq!(
            text("zone.state", json!({ "player": "RINCON_X", "track": null })),
            "21:04:05 RINCON_X: the queue ended\n"
        );
        assert_eq!(
            text(
                "player.health",
                json!({ "room": "Den", "health": "offline" })
            ),
            "21:04:05 Den: offline\n"
        );
        assert_eq!(
            text("topology.changed", json!({ "households": 2, "rooms": 5 })),
            "21:04:05 the rooms or groups changed: 5 rooms in 2 households now\n"
        );
        assert_eq!(
            text("events.reset", json!({ "oldest": 12 })),
            "21:04:05 events before #12 are gone: `fsonos zones` reads the house afresh\n"
        );
        assert_eq!(
            text(
                "zone.state",
                json!({ "room": "Kitchen", "source_changed": true })
            ),
            "21:04:05 Kitchen: source changed\n"
        );
        let work = |composer: &str, title: &str| {
            json!({ "composer": composer, "title": title, "movements": 1,
                    "reason": { "summary": "not heard in 4 days" } })
        };
        let status = |running: bool| {
            json!({
                "player": "RINCON_X", "zone": "Den", "running": running,
                "now": work("Bach", "Goldberg Variations"),
                "next": [work("Miles Davis", "So What")],
                "steering": { "source": "none" }
            })
        };
        assert_eq!(
            text("dj.status", status(true)),
            "21:04:05 Den: the DJ plays Bach: Goldberg Variations; next Miles Davis: So What\n"
        );
        assert_eq!(
            text("dj.status", status(false)),
            "21:04:05 Den: the DJ is off\n"
        );
        let logged = text(
            "action.logged",
            json!({ "id": 3, "at": 0, "client": "cli", "surface": "http",
                    "intent": "set_volume Kitchen 30", "decision": "allow",
                    "result": "done", "undoable": true }),
        );
        assert!(logged.starts_with("21:04:05 logged #3 "), "{logged}");
        assert!(
            logged.ends_with("set_volume Kitchen 30 -> done [allow]\n"),
            "{logged}"
        );
        assert_eq!(
            text("later.kind", json!({ "a": 1 })),
            "21:04:05 later.kind: {\"a\":1}\n"
        );
    }
}
