# Event delivery for browsers and agents

`GET /events` still sends numbered server-sent events. `Last-Event-ID` or
`since` resumes after a cursor. `GET /events?wait=30` returns one JSON event
batch; retain `last_id` as the next request's `since`. The existing event
names, payloads and HTTP authorization remain unchanged.

## One journal, shared wakeups

SSE readers consume the retained journal when their HTTP transport asks for
another frame. They do not copy an event queue or start a waiting thread per
connection. One lazily started deadline worker per event bus services idle
heartbeats and waiting HTTP long-poll deadlines. Publication wakes parked
readers directly; the worker retires after the last admitted reader leaves.

The journal retains 1,024 events. A reader that falls behind receives
`events.reset` followed by the events still retained. Read the house state
again after a reset: replay cannot recover events already evicted. Each
reader keeps its own cursor, so a stalled reader does not delay another.
Heartbeats do not consume event IDs. Long-poll reset markers, returned events
and final cursors are taken from one locked journal observation.

SSE and waiting HTTP long polls share a budget of 128 readers per bus. Once
full, another waiting reader gets HTTP 503 (`NOT_READY`) and `Retry-After: 1`,
not an empty successful response. Ready and zero-wait JSON reads need no
reader slot. Other control endpoints do not share this budget. A failure to
start the deadline worker also fails admission without consuming a slot.

Dropping a streaming body immediately releases its reader slot and parked
waker. When a disconnected socket causes the HTTP server to drop its body is
a transport concern; this change does not promise immediate detection of a
silent network partition. A wake already extracted by a publisher may finish
concurrently with drop. No event delivery operation writes to a speaker.

## Compatibility and limits

SSE comments keep the existing 15-second heartbeat interval; HTTP long polls
keep the existing 60-second maximum. `Surface::events_after` remains a
synchronous API for existing tool callers. It uses the same coherent snapshot
logic, but a caller choosing that synchronous API still blocks its own thread.

Rust callers can use `try_stream` to receive admission errors before starting
a response. The older infallible `stream` API remains available; admission
failure produces one `events.unavailable` frame and then ends.

Cursors remain numeric and process-local, not durable journal offsets or
proof of a daemon identity. An out-of-range cursor, including the maximum
u64 value, produces a reset without arithmetic overflow. A restarted daemon
that happens to reuse an in-range number cannot be distinguished by that
number alone. Reconcile current state when a connection opens, as the web
remote does. This does not implement MCP resource subscriptions.

## Regression coverage

The Rust unit tests cover deadlines, publication wakes, replaced wakers,
reader admission, prompt body cleanup, replay, lag resets, coherent JSON
snapshots, timeout clamping and terminal one-shot responses. The loopback HTTP
tests exercise actual fastapi/asupersync response streaming, simultaneous
readers, long-poll wakeup, cursor reset, and capacity errors while health and
immediate reads remain available. They use synthetic event publication, not
physical Sonos speakers or a full deployed tailnet.

Run `cargo test -p fsonos-api --lib events` and
`cargo test -p fsonos-api --test event_delivery_http`. A committed test is not
evidence of a passing run; inspect a recorded verifying run or run the commands
above.
