# Listening Room live recovery

The `/dashboard` Listening Room uses the same heartbeat-aware event transport
as the full remote. It reads the existing `/events` endpoint; no new service,
command endpoint or browser permission is introduced.

## What the user sees

A healthy connection and a completed snapshot produce **Live from your daemon**.
Opening a stream alone does not establish a current household observation.
Disconnects and journal resets invalidate old playback observations and pending
reviews. Late replies from the previous connection cannot make them fresh again.
A successful replacement snapshot restores the live display.

The shared transport observes heartbeat comments. It bounds connection setup to
15 seconds and complete-frame inactivity to 45 seconds, then reconnects with the
last complete event cursor. Reset events clear the previous journal's cursor.
The dashboard handles the daemon's actual `events.reset` event, even when the
reset is the only event delivered.

A quiet, healthy stream causes no periodic household reads. Events coalesce with
a fixed 350 ms deadline; a slow snapshot can have only one subsequent refresh
pending. Failed reads and unavailable event delivery use serialized read-only
fallback with backoff. Server Retry-After minima survive visibility changes,
reconnects and explicit Refresh clicks. An unavailable stream is never labelled
live merely because a fallback snapshot succeeded.

Room state, player health and DJ events refresh the affected listening groups.
An event naming a member resolves to its coordinator; a name present in several
households refreshes every matching group with an explicit household address.
Other groups retain their observations and queue selections. Unknown or malformed
identities, topology changes and journal resets require a full reconciliation.
`action.logged` also requires a full reconciliation: its human-readable intent
does not provide a structured target that the browser can safely narrow.

An offline group's recovery checks topology and rereads stale groups without
restamping healthy observations. Healthy room updates can proceed during that
group's backoff; a daemon-wide Retry-After still gates all reads. A partial room
failure remains visible without discarding event continuity for healthy rooms.

## Commands and reviews

Freshness is invalidated before recovery, including any dependent command
preflight. The JSON adapter cancels obsolete reads independently of whether a
stalled response body honors abort. Already submitted commands are not cancelled,
retried or replayed by event recovery. Their eventual acknowledgement remains
visible; lost or incomplete acknowledgements retain the existing uncertain
outcome until a successful explicit Refresh.

A command for an unchanged group can take priority over a background read of
another group. The browser retires and cancels only that observation before
claiming the command lane, then queues the interrupted groups for another read.
Late responses cannot overwrite state or release a newer read or command lane.
Transport, DJ and queue actions refresh their group afterward; scenes and undo
retain full-house reconciliation. Full reconciliation and a read of the selected
group continue to block control until the required observations are available.

A quiet live snapshot remains usable while event continuity is healthy; every
command still performs its existing native preflight. Without event continuity,
fallback observations retain their 30-second control age limit. DJ actions read
current session state on demand before constructing a review, so removing the
periodic poll does not leave DJ controls unusable after 45 quiet seconds. A
room change during that read cannot retarget the pending review.

Ordinary state events retain an immutable review and require fresh preflight
before confirmation. Connection loss, journal reset and suspension discard it.
Scene, library, queue, policy and single-command semantics remain owned by the
existing models and daemon. Design-preview mode opens no event connection and
sends no network reads or commands.

## Verification

```sh
node --test scripts/test-dashboard-recovery.cjs scripts/test-dashboard-live-integration.cjs scripts/test-dashboard-room-recovery.cjs
node --test --test-name-pattern='real loopback HTTP|throttled read' scripts/test-dashboard-http.cjs
python scripts/test-dashboard-recovery-component.py --chromium /path/to/chromium
```

The Node suites exercise the actual dashboard page binding, shared heartbeat
transport, request adapter and models with native Node Streams and fixture
HTTP/time. They cover both command/event orderings, stalled response bodies,
late cancelled responses, shared group members and duplicate names across
households, scoped queue invalidation, failed-room recovery, server cooldowns,
and explicit acknowledgement of uncertain writes. Real localhost HTTP cases
also check cancellation and single dispatch.

Ten Chromium component cases execute the shipped page script, dashboard models,
reconciler and shared event transport together. They use native DOM and Streams
in a minimal host, with fixture HTTP/time and unrelated visual/audio/workspace
collaborators. They cover quiet startup, reset, silent connection recovery,
hide/show, command and preflight races, on-demand DJ review, target changes,
explicit uncertain recovery, and offline design preview.

These checks do not establish full-dashboard layout, browser-to-daemon delivery,
real HTTP asset bundling, physical Sonos behavior or live Tailscale acceptance.
Rust changes only embed the shared transport and dashboard reconciler before the
page script. Rust compilation and the full existing test suite were not run in
the editing environment. Both new Node suites are registered in Linux CI.
