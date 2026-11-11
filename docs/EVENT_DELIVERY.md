# Event delivery for browsers and agents

`GET /events` still sends numbered server-sent events. `Last-Event-ID` or
`since` resumes after a cursor. `GET /events?wait=30` returns one JSON event
batch; retain `last_id` as the next request's `since`. HTTP authorization is
shared with the other daemon reads.

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

## Playback source transitions

A `zone.state` event includes `source_changed: true` when accepted playback
observations change the active AVTransport source, including clearing it.
A temporary announcement and the retained DJ queue can share a current-track
observation; the source transition still invalidates playback views. Read the
current zone state again. The marker does not publish a source address, imply
a track advance, or authorize a playback command. Existing consumers that
refresh on `zone.state` receive these transitions through the same journal.

## Native queue and catalog changes

The daemon subscribes to each coordinator's ContentDirectory events alongside
its playback events. A `zone.state` event with `queue_changed: true` asks readers
to discard old queue observations, including edits that kept the same number
of tracks while playback was paused. It carries no inferred queue length,
revision or track transition. Read the current queue again before using an
old occurrence, search result or reviewed edit.

`catalog.changed` identifies the native catalogs that need rereading through
the boolean `favorites` and `playlists` fields. Its `household`, when available,
uses the same label as the zone API (`S1`, `S2` or the disambiguated household
id). An absent household means readers cannot safely narrow the invalidation.
The event contains no catalog entries or playback command. Specific container
updates invalidate their catalog; an ambiguous system update or new
subscription invalidates all relevant native content. Repeated unchanged
counters do not generate new changes.

Both browser remotes refresh an open queue through their existing coalesced
read lane and retire pending queue edits immediately. The phone remote
refreshes affected household favorites with one fixed deadline per burst;
open native-playlist and favorite-search views discard stale selections and
ask for a fresh explicit read. An event arriving during an earlier catalog
read prevents that response from restoring old choices. Disconnects and
journal resets invalidate catalog observations too. These notifications never
resend a command or acknowledge an uncertain write. The DJ retires its cached
queue proof before reconciliation, and MCP event/resource consumers receive
the same journal changes.

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
remote does. MCP resource subscriptions also invalidate their documents when
the shared journal advances or loses continuity.

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

Native content regressions are in `events::content_tests` and
`surface::dj_follow::tests`. Browser coverage includes same-length paused
queue edits, household isolation, coalesced catalog reads, late-response
rejection and preserving the outcome of a command already sent:

```sh
node --test scripts/test-remote-catalogs.cjs scripts/test-remote-event-integration.cjs scripts/test-dashboard-live-integration.cjs scripts/test-sonos-playlist-track-actions.cjs
```
