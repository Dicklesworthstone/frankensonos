//! `fsonos events`: the house's changes as they happen, from a running
//! daemon's event stream (`GET /events`, see `fsonos_api::events`). A line
//! per change in words (a room's volume or play state, a player going
//! offline, a logged action, the DJ's picks), or with `--json` an object per
//! line (`{id, event, data}`) for a script. `--since <id>` first replays what
//! came after that event; `--count` stops after that many. Local and URL clients,
//! including Tailscale Serve HTTPS, share cursor-preserving bounded journal reads.

use clap::Args;
use fsonos_api::{ActionDto, DjStatusDto, DjWorkDto, ErrorCode, Failure};
use fsonos_core::clock::{Clock, SystemClock};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{self, Write};

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
/// going away, or a terminal read failure after bounded retries.
pub fn run(global: &GlobalArgs, args: &EventsArgs) -> anyhow::Result<()> {
    let Some(mut daemon) = Daemon::find(global, true)? else {
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
    let global = global.clone();
    polling::run(
        move |since, wait| {
            // A restarted local daemon may have a new listener or CLI token.
            // Reapply route confinement before each read, without another
            // health request or any discovery of the speaker LAN.
            daemon.refresh_local(&global)?;
            match daemon.events_after(since, wait) {
                Err(failure)
                    if failure.code == ErrorCode::PolicyDenied
                        && daemon.refresh_local(&global).unwrap_or(false) =>
                {
                    // The daemon can rotate between resolution and this GET.
                    // Retry only a proved new local identity; an unchanged
                    // policy refusal, URL, or failed route check stays final.
                    Err(Failure::new(
                        ErrorCode::NotReady,
                        "the local daemon changed while reading events",
                    ))
                }
                result => result,
            }
        },
        args.since,
        |event| output.show(&event),
    )
}

/// Local and remote readers print the same lines and count reset notices alike.
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
    match d.get("transport_status") {
        Some(Value::String(status)) if status == "ERROR_OCCURRED" => {
            said.push("playback error (ERROR_OCCURRED)".to_owned());
        }
        Some(Value::String(status)) => {
            said.push(format!("renderer status {}", status.escape_default()));
        }
        Some(Value::Null) => said.push("renderer status unknown".to_owned()),
        _ => {}
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

    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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

        let mut output = Output {
            writer: Closed,
            json: true,
            limit: None,
            shown: 0,
        };
        assert!(!output.show(&event).unwrap());
        assert_eq!(output.shown, 0);
    }

    #[test]
    fn renderer_status_events_distinguish_error_recovery_and_unknown() {
        for (status, expected) in [
            (json!("ERROR_OCCURRED"), "playback error (ERROR_OCCURRED)"),
            (json!("OK"), "renderer status OK"),
            (Value::Null, "renderer status unknown"),
            (json!("FUTURE_STATUS"), "renderer status FUTURE_STATUS"),
        ] {
            let event = Event {
                id: Some(1),
                kind: "zone.state".into(),
                data: json!({"room": "Room", "transport_status": status}),
            };
            assert_eq!(
                event_text(&event, "12:00:00"),
                format!("12:00:00 Room: {expected}\n")
            );
        }
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
