# Native live house state

`FrankenSonosKit` now supplies both a one-shot observation and a reconnecting,
read-only session. These build on the existing daemon API and URLSession
transport; no speaker credentials or new endpoints are involved.

## One-shot snapshot

```swift
let snapshot = try await client.houseSnapshot()
let id = RoomID(household: "EXAMPLE", room: "Example Room")
let observed = snapshot.states[id]
let unavailable = snapshot.failures[id]
```

Topology is validated first. Every group member gets its own household-qualified
state read, with at most four concurrent requests and a 1,024-room total limit.
An unreachable member appears in `failures`; healthy siblings remain available.
Failed refreshes remove the affected stale value rather than inventing a level.
A failed topology request throws instead of reporting an empty house.

These are observations made over several HTTP reads, not an atomic snapshot or
reservation against another controller. `ZoneState` is the playback observation;
`zones` is the topology that the per-room reads were validated against.

## Foreground live session

```swift
let live = LiveHouse(client: client)
let observer = Task {
    for await view in live.updates {
        // Publish view to the application's UI actor.
        // Display cached observations with their phase and failure labels.
        // view.isFresh is false during reconnect, refresh, outage and suspension.
    }
}
await live.resume()

// On background entry:
await live.suspend()
// On foreground entry:
await live.resume()
// When disposing this session:
await live.stop()
observer.cancel()
```

Resume is idempotent. Suspend cancels the stream, timers and outstanding reads,
retains the last observation as stale, and prevents obsolete results from
publishing. A rapid resume waits for an old read batch to drain before starting
another. Stop is terminal; create a new session after stopping or cancelling its
consumer.

Every SSE connection and reconnect requests a full house read, even when a
numeric `Last-Event-ID` is available. Room events coalesce for 250 ms and refresh
healthy members of all matching groups across households. Topology changes,
reset events, unknown room names and malformed room-event data request a full
read. Events arriving during a read cause a subsequent batch; the earlier result
is not marked fresh.

Stream failures, normal EOF and event-buffer overflow reconnect with bounded
exponential backoff (one second initially, at most one minute). Overflow and
`events.reset` discard the replay cursor. Read failures retry independently on
the same bounded backoff, even without another speaker event. A healthy, quiet
SSE connection is not polled. The pending room set is bounded at 64 names, after
which it becomes one full read.

### Isolated room recovery

An unavailable room no longer holds healthy-room events behind its retry timer.
Healthy members, including siblings of a failed coordinator and same-named rooms
in another household, continue to refresh. Those reads skip failed members and
neither reset nor postpone their recovery deadline. A failed room's event does
not cause another read of that failed room before the deadline. Automatic recovery
and event refreshes share the same single read slot and four-request fanout cap.

At the deadline, recovery reads current topology and retries missing observations.
Unchanged healthy groups retain their values and original per-room timestamps;
new or regrouped rooms are read again, and departed rooms and failures disappear.
Queued healthy events are included or followed by another batch, never lost.
A topology-read failure or pending full invalidation blocks targeted reads until
a full read is possible: cached membership is not treated as current evidence.
An explicit `refresh()` may override the delay; cancelling a retry invalidates
its identity so an old callback cannot restart it.

The whole-house `isFresh` flag remains false while any room is unavailable.
Healthy values are still delivered, but neither a missing level nor an uncertain
write is converted into success. Recovery does not restart a speaker, send a
playback command, or change the separate command-review preflight.

The output buffers only the newest **complete view**, never reconstruction
deltas. A slow UI can skip intermediate display states without losing the state
needed for the latest view. Each view includes cached data, freshness phase and
failure information. The raw transport's separate 128-event buffer is unchanged.

This is not a command queue or acknowledgement mechanism. It performs only GETs
and SSE subscription. An uncertain write remains uncertain; receipt handling and
fresh command preflight remain the caller's responsibility. The layer observes
playback and member levels; DJ detail, favorites and queue pages remain available
through their existing `DaemonClient` read methods. No native screen is added.

## Verification

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
The snapshot and session tests execute the production client and actors using
controlled transports, including failed reads, delayed cancellation, cursor
reset, same-name households and dropped event streams. The package's existing
model, parser, request and command tests run alongside them. These tests do not
claim Apple URLSession.AsyncBytes or physical-Sonos acceptance from a Linux run;
the existing macOS binary workflow separately runs the Swift package on Apple.

The isolated-recovery change adds eight `HouseRecoveryTests` and thirteen
`LiveHouseRecoveryTests`. These and the sixteen existing live-session tests pass
with warnings treated as errors in a focused Swift 6.2.1/Linux harness containing
the exact snapshot/live/parser files and verbatim read-only client/model
declarations. Unrelated command and account methods are excluded; this is not a
full-package integration result. The existing backoff test now checks that the
failed room is not reread, while allowing its healthy group members to refresh.
Four new regression cases were also run against the prior live scheduler and
failed there. A real loopback HTTP fixture exercised the production URLSession
read path: nine GETs covered an outage, targeted recovery and regrouping without
rereading an unrelated household. Apple SSE transport, native rendering and
physical-speaker behavior still need their own acceptance checks.
