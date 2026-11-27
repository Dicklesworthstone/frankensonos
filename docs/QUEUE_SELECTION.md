# Copy selected tracks from a live Sonos queue

`queue-selection` copies 1–10 expanded native queue occurrences to a room,
saved room alias, or every existing group in the same household. Positions
are one-based; the supplied order and repetitions are retained. For example,
`3 1 3` copies the third occurrence, then the first, then the third again.

The source queue can be read while another source plays. Native resource and
service metadata come from the speaker. No Spotify Web API sign-in is needed
for an already replayable native selection. Incomplete Spotify queue metadata
can use the current queue track's observed account and service identity, or
the household's own favorites; renderer flags are not required for native
metadata repair.

## CLI

Read the source queue and its current revision:

```sh
fsonos queue 'Den@S2' --json
```

Copy the returned `revision` unchanged into the selection command:

```sh
fsonos queue-selection 'Den@S2' 3 1 3 \
  --target 'Office@S2' \
  --revision '<revision returned above>' \
  --mode play
```

`queue-search` also prints a usable `Revision:` value. `--expected-revision`
is an alias for `--revision`.

| Mode | Destination behavior |
|---|---|
| `play` | Stage and verify the selected tracks, replace the queue, then play the first occurrence using the existing playback policy and volume caps. |
| `next` | Insert after the currently selected queue occurrence; append if the group is playing another source. Playback and volume remain unchanged. |
| `append` | Add the ordered selection to the end without starting or resuming playback. |

Omit `--target` to act on the source group. A target such as `all@S2` or a
saved alias expands to complete, deduplicated existing groups. If the source
group is included, it runs last so its queue remains available for the other
copies. If the source is excluded, its queue and playback are preserved.

## HTTP and MCP

`POST /queue/selection` accepts the same operation:

```json
{
  "zone": "Den@S2",
  "target": "Office@S2",
  "positions": [3, 1, 3],
  "expected_revision": "<source queue revision>",
  "mode": "next"
}
```

The MCP tool is `queue_selection`, with the same fields. `zone` always names
the original queue reader; `target` names the destinations. A supplied
`expected_target_revision` binds the physical audience from `/rooms/resolve`.
An optional `expected_queue_revision` binds a single destination queue and
is rejected for a target with multiple groups.

The source revision, selected native occurrences, original group, and any
account evidence used for repair are rechecked before destination writes.
Cross-household selections are rejected. Each destination uses the existing
native queue executor, with its staged replacement or verified insertion and
undo handling.

Responses retain the original positions and titles. Playback uses `batch`
receipts when needed; insertion uses `queue_batch`. Partial results identify
the groups that succeeded, failed, or were not attempted. An uncertain write
is never automatically repeated: read the affected queues before choosing
another action.
