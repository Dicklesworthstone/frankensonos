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

## Browser remote and dashboard

Select up to ten readable occurrences in either browser queue view and choose
**Copy selected to rooms…**. The copy panel retains the original queue and
positions, including selections gathered across queue pages or search
results. Move, repeat, or remove entries to choose the copy order. These
controls change only the draft; they do not rearrange the source queue.

Choose a destination room, saved alias, or `all@household`, then choose
**Play now**, **Play next**, or **Append**. **Review copy** resolves the complete
destination groups and shows their current playback and queue effects before
the separate confirmation. Mixed-household destinations are rejected. A
qualified member room remains the original source even when another member
coordinates its group.

The browser handoff is `fsonos:queue-selection` with
`detail.selection = { zone, revision, total, tracks }`. Every track retains
`position`, `title`, `uri`, `creator`, and `album` from the observed queue;
ordering and repetitions remain significant. `QueueCopyRemote.selectionOf`
accepts 1–10 occurrences and rejects caller-supplied DIDL or other metadata.
`open(selection)` performs source reads and returns a promise of whether the
source was verified; opening a panel never writes.

The shared model rereads every unique selected position with the original
queue revision and total. It compares the resource and displayed track
fields, checks the source against `/zones`, and brackets those reads with
`/rooms/resolve` physical-group proofs. Each review also reads every
destination's playback and queue, then resolves its audience again. The
45-second confirmation window begins when those reads finish. Confirmation
repeats the source and destination checks inside the shared command lane.
The lane reserves full destination groups plus a distinct source group for
reads; the source appears among the affected destinations only when the
user actually included it.

Confirmation sends exactly one `POST /queue/selection`. Its body contains
the original `zone`, selected `positions`, `expected_revision`, resolved
`target`, `expected_target_revision`, and mode (`end` becomes `append`). A
single destination also includes `expected_queue_revision`. Titles, resource
URIs, and metadata are never submitted as replacement playback content.

`pending.targets` is the shared `SourceTargetsRemote.inspect` result;
`pending.mode`, `pending.selection`, and the pending object itself remain
stable until consumed or invalidated. The shared target adapter uses the
explicit `native_queue_tracks` kind and validates the response's original
`source_zone`, destination, positions, titles, mode, and complete per-group
playback or queue receipts. Multi-group insertions retain the speaker's
actual confirmed offsets if queues grew after review. The execution callback
returns the validated receipt with `detail`, `complete`, `partial`,
`uncertain`, and the daemon's actual `changed` flag.

Editing, closing, suspension, stale callbacks, expired review, or changes to
source or destination state prevent a late write. A dispatched operation
keeps its eventual acknowledgement or uncertain outcome even if the panel
closes. Failed and partial results keep each destination's details; the
browser never retries the request or a subset automatically.

Run `node --test scripts/test-native-queue-copy.cjs scripts/test-queue-copy-panel.cjs`
for production-model, loopback HTTP and modeled-DOM coverage. The panel tests
drive both actual queue handoff buttons, ordered draft controls, destination
review, the dashboard mutation lane and navigation during reads or writes.
