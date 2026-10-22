# Removing entries from a group queue

The HTTP API and MCP use the same `remove_from_queue` action. It removes
one to 100 consecutive queue entries without clearing and rebuilding the
queue or issuing play, seek, source, volume, mute or grouping commands.
The player may advance or stop when its currently playing entry is removed.

**Check the response's `undoable` field.** The daemon can save a complete
backup of a selected queue of up to 1,000 entries / 4 MiB and restore it with
`undo_last`, including its original selected occurrence, position and play
mode. Undo requires the queue, playback and physical group to still match
the confirmed edit. Unsupported captures and uncertain writes remain explicit
undo barriers. See [queue edit undo](QUEUE_UNDO.md) for the bounds and checks.

## Read, select, remove

First read `GET /zones/{room}/queue` or MCP `get_queue`. Take the selected
entries' one-based positions and exact `uri` strings. For a consecutive
range, supply its first position and every selected URI in order. Copy the
response's opaque `revision` token too: do not parse or construct it.

Send `POST /queue/remove` with `Content-Type: application/json`:

```json
{
  "zone": "Room@S2",
  "position": 3,
  "expected_uris": [
    "x-file-cifs://example.invalid/selected.flac",
    "x-file-cifs://example.invalid/next.flac"
  ],
  "expected_revision": "<copy the revision from the queue response>"
}
```

For MCP, call `remove_from_queue` with the same four arguments. The stdio
backend prefers a running daemon, sharing its policy and action log. Once
a daemon answers or fails, the backend does not attempt the deletion again
on the local LAN. Without a daemon, the local surface enforces the same
identity and policy checks.

The URIs above are synthetic placeholders. For Spotify, copy the **renderer
URI returned by the queue**, including its encoding and parameters, rather
than substituting a `spotify:track:` catalog URI. A range is not a set: order
and repeated entries matter. `expected_uris` is required. `expected_revision`
is optional for compatibility, but should be supplied to reject intervening
queue edits or a different coordinator even when the selected URIs look the
same. Across pages, use only entries with matching revision tokens.

An example successful response when no complete undo backup was saved:

```json
{
  "done": "removed 2 track(s) at position 3 from Room's queue; 8 remain (not undoable)",
  "zone": "Room",
  "position": 3,
  "count": 2,
  "remaining": 8,
  "recovered": false,
  "undoable": false
}
```

This action has no dedicated CLI command yet. `fsonos queue Room --json`
can supply the selection for an HTTP or MCP client. Removing something from
the queue does not ban it from future DJ picks or stop an active DJ.

## Conflicts and lost replies

The shared action resolves fresh group topology, reads a coherent complete
queue, and compares the selected identities and optional revision. It sends
one native `RemoveTrackRangeFromQueue` using that queue's `UpdateID`, then
reads the queue again. It never falls back to `RemoveAllTracksFromQueue`,
re-enqueueing everything, or repeating an uncertain deletion.

Native revision zero is a wildcard, not a revision check; the action refuses
it without writing. A valid nonzero revision protects against an intervening
queue edit at the player. Topology is checked again before dispatch, but
there is no atomic transaction spanning topology and queue operations.
Revision tokens are observations, not permanent identities across reboots
or counter wraparound.

If a reply is lost or malformed, success requires the new queue to match
exactly the original prefix and suffix with just the selected range removed,
at a different revision. That result reports `recovered: true`. Concurrent
edits, failed read-back or a timeout with unchanged reads are **uncertain**:
`OUTCOME_UNCERTAIN` with `retryable: false`. Read the current queue before deciding
what to do next. Do not blindly repeat the request; the original write could
already have happened or still be finishing.

A stale selection is rejected before removal as `QUEUE_CHANGED` (not
retryable: read the queue again). A player that refuses the old revision
(UPnP 1028, a concurrent edit just before ours) is reported the same way,
since nothing was removed. Denied
callers receive `POLICY_DENIED`. HTTP retains the normal JSON and Origin
checks. All outcomes reaching the action are logged; malformed HTTP bodies
are rejected at the request boundary.

## Verification coverage

The implementation includes pure selection/observation tests, real-loopback
S1/S2 simulator cases, HTTP read-remove-conflict and request-boundary tests,
and MCP daemon-forwarding/error tests. Local source/hash/model checks do not
prove those Rust tests passed. Physical-Sonos behavior remains a separate
hardware qualification step.
