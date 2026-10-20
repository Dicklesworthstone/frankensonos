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

## Commands and reviews

Freshness is invalidated before recovery, including any dependent command
preflight. The JSON adapter cancels obsolete reads independently of whether a
stalled response body honors abort. Already submitted commands are not cancelled,
retried or replayed by event recovery. Their eventual acknowledgement remains
visible; lost or incomplete acknowledgements retain the existing uncertain
outcome until a successful explicit Refresh.

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
node --test scripts/test-dashboard-recovery.cjs scripts/test-dashboard-live-integration.cjs
node --test --test-name-pattern='real loopback HTTP|throttled read' scripts/test-dashboard-http.cjs
python scripts/test-dashboard-recovery-component.py --chromium /path/to/chromium
```

The new Node suites contain 42 passing tests. One uses a real localhost HTTP
server to show that cancelling a stalled read leaves a submitted command alive
and single-dispatch. Twelve integration checks cover the actual dashboard page
binding, shared heartbeat transport, request adapter and models, with native
Node Streams and fixture HTTP/time. The six selected existing HTTP tests pass
unchanged; five use real localhost sockets. The two unselected asset tests in
that existing file were not run locally.

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
