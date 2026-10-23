# Move entries in a queue

`POST /queue/move` and MCP `move_in_queue` move a consecutive block using
Sonos's native `ReorderTracksInQueue` action. They do not clear or rebuild the
queue, re-enqueue tracks, change the source or play mode, seek, or issue Play.
The shared policy action is `move_in_queue`; unknown clients remain read-only.

Read the room's queue first with `GET /zones/{room}/queue` or MCP `get_queue`.
Keep that response's `revision` and the exact renderer `uri` of each selected
entry. A move requires both; it will not silently act on a newer observation.

## Phone remote

Open **Queue** on the room's card. Every entry has **Move…**, with **Move earlier**,
**Move later**, **After current entry**, **To start…**, and **To end…**. A row's
actions move only that occurrence, including when the same recording appears
elsewhere. No position arithmetic or checkbox selection is needed. The menu
works with touch and keyboard; Escape closes it and returns focus to the entry.

Select consecutive entries to get the same choices in **Move selected entries**.
This keeps the selected block together, including up to 100 entries captured
across pages with [range selection](QUEUE_RANGES.md). Earlier/later shifts the
block one place without changing its internal order. **After current entry**
uses the current queue position in the observed snapshot; it is unavailable
when no current entry is known, the selection includes it, or the block is
already next. Boundary actions are disabled when they would not change order.

For another destination, expand **Position & range tools** below the queue,
enter a one-based **Move selected entries before original position**, and
choose **Review move**. The destination can be anywhere in the full queue,
not just the displayed page. The review names
the room, selected titles, destination, and resulting first position. Only
**Move entries** sends the request; **Cancel move** leaves the queue alone.
Cancelling a shortcut review restores keyboard focus to its entry or selection
control. Choosing any destination prepares a review; it never starts playback.
The confirmation appears above the queue, outside every collapsible tool, and
comes into view when a destination is chosen.

The browser keeps exact renderer identities, including duplicate tracks, and
the observed revision. After success it reads the page containing the moved
block. A second edit therefore starts from a new observation. Moving within
the selected block or immediately after it is a no-op with no request.

Changing the selection, refreshing, paging, or closing the panel discards the
review. A replaced room card cannot send its old confirmation. A request
already sent is never retried or silently cancelled on closing; reopen the
panel to inspect its outcome. Lost or unreadable replies require an explicit
refresh before another edit. The browser does not clear/rebuild the queue or
start playback. The result reports whether the daemon saved a complete undo
backup; [queue edit undo](QUEUE_UNDO.md) explains its bounds and checks.

For terminal use, `fsonos queue-move` accepts a saved queue page or explicit
URI/revision guards; see [CLI_QUEUE_EDIT.md](CLI_QUEUE_EDIT.md).

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
confirmed the expected new order. `undoable` is true only when the daemon saved
the complete original selected queue and proved the exact post-move result.
Queues outside the backup bounds and uncertain writes remain undo barriers.
Read the current queue again before selecting another block; subsequent queue
or playback changes can prevent the earlier move from being undone.

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

The browser tests execute the shipped queue model and fetch client, including
all block/destination combinations for a six-entry queue, duplicate identities,
cross-page moves, stale reviews, and uncertain replies. Run them without extra
packages:

```sh
node --test scripts/test-remote-queue.cjs
```

Client tests and localhost HTTP fixtures do not establish physical-speaker
behavior. Check the matching Rust CI and opt-in live-device results separately.
