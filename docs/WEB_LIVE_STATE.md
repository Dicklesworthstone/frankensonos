# Live remote recovery

The room cards recover their state after a lost event connection, daemon
restart, event-history reset, or return to a hidden tab. A successful event
connection is followed by a full state read; the header says **Synchronizing**
until that read completes, rather than treating an open socket as fresh state.

While events are unavailable, reads run approximately ten seconds after the
previous batch completes. Read failures increase that delay to twenty, forty,
and then sixty seconds. Successful reconciliation returns to events alone.
A healthy, quiet stream does not cause periodic household polls. Hidden pages
close their event stream and stop retry timers; returning opens a new stream
and reads current state. Browser GET requests have a fifteen-second deadline,
including the response body, so a stuck read cannot hold recovery forever.
These deadlines do not apply to writes and do not cause writes to be replayed.

Event bursts are combined on fixed 300-millisecond deadlines, not indefinitely
postponed by each new notification. Only one event read batch runs at once.
Pending room names are bounded; an overflow, unknown room, topology change,
or reset prompts a full read. A full batch uses at most four group workers,
with each group's member reads sequential. Direct user requests can run
independently of the event batch.

Event room names alone do not identify a household. The remote reads all
matching groups by household-qualified names; it never paints a received
volume or mute value onto the first same-name room. Unknown or failed playback
reads leave the last observation visible, labelled unavailable, rather than
inventing stopped playback or an empty house.

## Keeping controls usable during recovery

A refresh with unchanged group membership retains the existing room card,
including its open queue, draft input, selection, review, scroll and keyboard
focus. Changed groups get new cards; other groups retain theirs. Queue entries
remain snapshots, not live positions: backend revision checks still reject
selections changed by another controller.

Older topology, playback and level responses cannot overwrite newer reads or
paint into a disposed card. Replacing a card cancels its unsent slider timer.
Detached controls cannot send requests. Playback/DJ controls in one card and
mute controls in one room reject overlapping submissions before creating a
request. Volume reads do not cancel a deliberate debounced slider change.
Already-dispatched requests are not undone or automatically repeated.

These are browser lifecycle and observation guarantees, not an atomic device
transaction. The daemon still owns policy, coordinator resolution, action
logging, revision checks and speaker control. A socket silently stalled while
still reported OPEN is subject to the browser's own network-error detection;
SSE heartbeat comments are not exposed as JavaScript events.

## Executed checks

Run `node --test scripts/test-remote-live.cjs scripts/test-remote-state.cjs`.
The recovery suite uses a virtual clock and event source; the room-card suite
uses a minimal DOM and synthetic responses while executing the shipped code.
Offline Chromium interaction checks additionally exercise real DOM identity,
focus, household routing and delayed-control cleanup. These are not tests of a
Rust daemon, a complete remote with every panel, or physical Sonos speakers.
