# Web remote event continuity

The full remote at `/` (including its Studio layout) consumes the daemon's
existing `/events` stream through `RemoteEventFeed`. A connection cannot remain
apparently live forever merely because the browser has not reported an error.
The server's existing fifteen-second heartbeat comments count as activity;
no backend route, event schema, subscription worker or polling loop was added.

Connection establishment has a fifteen-second deadline. Once headers arrive,
forty-five seconds without a complete SSE block closes the reader and reports
a connection error to the existing live-state reconciler. Merely receiving
fragments cannot postpone that deadline indefinitely. Browser suspension or
operating-system scheduling can delay timers; these are client budgets, not a
real-time guarantee. A quiet healthy stream does not trigger state polling.

Reconnection retains the last completely received decimal u64 cursor as a
string, without JavaScript numeric rounding. An incomplete block at EOF is
not dispatched and cannot advance the cursor. A journal reset clears the old
cursor even when the new daemon has not published a numbered event yet. Invalid
cursors and oversized incomplete blocks discard resume state; reopening then
requires a fresh snapshot. Frame memory is limited to 1 Mi UTF-16 code units.
The parser handles incremental UTF-8, CR/LF/CRLF, BOM, multiline data, comments,
empty IDs and server retry instructions.

Retries use an exponential base from one to sixty seconds plus jitter, subject
to the server's longer minimum delay. `Retry-After` survives closing/replacing
a feed in the same loaded page; extremely large hints do not overflow into a
rapid retry loop. A full document reload is not persistent rate-limit storage.
Only same-origin `GET /events` is issued. Redirects are refused.

## Observation versus command lifetime

Connection loss, resubscription, journal reset and page suspension invalidate
older topology, card and member-level read generations and cancel outstanding
JSON reads through the existing request adapter. A late reply cannot repaint
a pre-disconnect observation as fresh. Queued members of an obsolete batch do
not start. Reconnection earns the Live label only after its new full snapshot
completes; partial failure retains the existing bounded recovery behavior.

Already sent POST/PUT commands are neither cancelled nor replayed. A pending
read-only command preflight is cancelled before its dependent write can start.
This is not a server-side transaction or a cross-tab command lock. Native
EventSource remains the fallback for standalone embeds or browsers without
fetch streaming; without either, the pre-existing read fallback remains.
The separate Listening Room dashboard and other independently mounted streams
are not migrated by this change.

## Reproduce the software checks

```sh
node --test scripts/test-remote-event-feed.cjs \
  scripts/test-remote-event-integration.cjs scripts/test-remote-live.cjs \
  scripts/test-remote-invalidation.cjs
python3 scripts/test-remote-event-browser.py --chromium /path/to/chromium
```

The first command covers 94 tests: 41 transport, 20 integration and 33 existing
live-client/invalidation regressions. Three transport tests use real loopback HTTP sockets
for reconnect/cursor delivery, stalled-body cancellation and redirect refusal.
Deadlines are advanced deterministically rather than spending forty-five real
seconds per failure. Integration tests execute the actual request broker,
request adapter, live reconciler and page lifecycle with controlled transport.

The six Chromium component cases run the shipped bootstrap, native browser
Streams, decoding and DOM. HTTP responses and time are fixtures; they do not
establish browser-to-daemon or Tailscale acceptance. Rust changes only embed the
helper ahead of the existing remote and include it in the privacy checks.
Workspace compilation, full-page asset delivery, physical Sonos and live-tailnet
behavior remain separate checks; no new claim about those is made here.
