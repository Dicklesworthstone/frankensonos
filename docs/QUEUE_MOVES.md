# Move entries in a queue

`POST /queue/move` and MCP `move_in_queue` move a consecutive block using
Sonos's native `ReorderTracksInQueue` action. They do not clear or rebuild the
queue, re-enqueue tracks, change the source or play mode, seek, or issue Play.
The shared policy action is `move_in_queue`; unknown clients remain read-only.

Read the room's queue first with `GET /zones/{room}/queue` or MCP `get_queue`.
Keep that response's `revision` and the exact renderer `uri` of each selected
entry. A move requires both; it will not silently act on a newer observation.

## Request

Send JSON with `Content-Type: application/json` to `/queue/move`, or pass the
same fields to MCP `move_in_queue`:

```json
{
  "zone": "Room",
  "position": 2,
  "insert_before": 6,
  "expected_uris": [
    "x-file-cifs://example.invalid/b.flac",
    "x-file-cifs://example.invalid/c.flac"
  ],
  "expected_revision": "<revision from get_queue>"
}
```

The URIs above are illustrative, not values to substitute for the queue's
actual renderer identities. One request moves one to 100 consecutive entries.
A room can be a group member; the action resolves its current coordinator.

**Both positions describe the original queue.** For `A B C D E`, selecting
`B C` at position 2 gives:

| `insert_before` | Result | Returned `position` |
|---|---|---|
| 1 | B C A D E | 1 |
| 2, 3, or 4 | A B C D E, no write | 2 |
| 5 | A D B C E | 3 |
| 6, the original total + 1 | A D E B C | 4 |

The destination is not an index in the shortened queue after extraction.
Out-of-range values are rejected, not clamped to another track. The moved
entries keep their internal order.

## Result and subsequent edits

A successful response contains `done`, `zone`, the block's new one-based
`position`, `count`, unchanged `total`, and the observed post-operation
`revision`. `changed` is false for structural no-ops, which send no native
write. `recovered` means the write reply failed but a complete coherent read
confirmed the expected new order. `undoable` is false: an ordinary playback
snapshot does not capture enough queue identity to reverse this edit safely.
Read the current queue again before selecting another block.

Each real move carries the observed nonzero native UpdateID. A reported zero
is refused rather than used as a wildcard. The controller checks fresh group
membership and reads the entire bounded queue before and after a move. A
stale caller revision is rejected before the native mutation.

There is **no automatic retry** of the mutation. A lost or malformed reply
can be reconciled by read-back, but a failed read, a conflicting edit, or an
unverifiable result returns an uncertain, nonretryable error. Identical
renderer URIs alone cannot prove which duplicate occurrence moved after a
lost reply; that case remains uncertain. Never discard the revision or replay
the original request blindly. Inspect the queue and make a new decision.

## Scope and verification

The revision is an observation, not a lock, a transaction spanning reads and
writes, or a permanent device epoch. Another controller can change the queue
again; counters can wrap or reset. Native UpdateID enforcement and playback
continuity on physical S1/S2 firmware still require live-device acceptance.
The simulator models those behaviors; it is not physical-hardware evidence.

Regression coverage is in `fsonos-proto::queue`, the simulator's queue state
transition and `queue_reorder` socket tests, API `queue_move_sim` and
`queue_remove_http`, and MCP queue forwarding tests. Rust compilation and test
execution were not available in the implementation environment.

Queue moves are currently exposed through HTTP and MCP. There is no dedicated
CLI queue-move subcommand or automatic queue-move undo. `fsonos queue --json`
can still supply the observation for an HTTP or agent client.
