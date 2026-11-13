# Play or queue an ordered selection of tracks

Choose a few movements from a cached album, several entries from a playlist,
or individual library tracks and play or queue them in one action.
The selection contains **1–10 tracks or episodes**, preserves their supplied
order and repeated occurrences. **Play selected now** replaces the selected
group's queue and starts the first track in normal order. Queue-next and append
leave current playback alone. All three operations share the existing policy,
action history and verified undo boundaries.

## From the phone remote or Listening Room

Open **Find music** and browse cached tracks, or open an album or playlist with
**View cached tracks**. Mark **Select track** beside the wanted entries.
Both clients keep selected entries in their listing order, regardless of the
order in which you check the boxes. Selecting two occurrences of the same URI
keeps both; a disc/track position or original playlist position distinguishes
them in the review.

Choose an explicit listening group, then **Play selected now**, **Queue selected
next** or **Add selected to end**. The review shows the complete ordered list,
source URIs, destination and effect. Play-now explicitly reviews replacing the
existing queue and starting the selected music. Confirm to send the selection
once. Queue-next places
the list after the current queue occurrence. When a group is playing another
source, it appends to the queue without activating it. Paused playback stays
paused, and queueing does not raise volume.

Selection is limited to the current 50-entry catalog page and at most ten
entries. Changing its filter, page, category or collection clears selection;
closing the chooser also clears it. You can browse and mark cached tracks
without online speakers, but review requires a fresh explicit destination and
readable queue. The phone retains the selected music when you choose another
group and requires a new review for that group.

At confirmation the clients reread the selected catalog observation, exact
occurrences, destination group, playback source and destination queue. A changed
listing, group, source or queue asks for another review. Queue-next also checks
that the current queue occurrence has not changed. These checks are optimistic
observations; they do not reserve speakers against another controller. The
server additionally retains and verifies its own destination through insertion
or staged replacement. Play-now also checks the reviewed current occurrence.

Only the checked cached entries are submitted. This does not fetch missing
album tracks or expand a playlist. The catalog's complete/subset/partial
coverage and unknown explicit metadata keep their existing meanings. Use a
container row's existing **Play now** or **Add to end** action when you want the
entire current album or playlist source.

## HTTP and MCP

### Start the selected tracks

`POST /play/tracks` accepts `zone`, the same ordered `tracks` array shown below,
and optional `expected_revision`. It does not accept `next`; choosing this
endpoint explicitly requests queue replacement and playback from the first
selected occurrence. It returns the ordinary playback outcome:
`done`, `changed`, and `notes`, including applied volume limits and any logging
or undo limitations. The OpenAPI operation and Full-profile MCP tool are both
named **`play_tracks`**.

It uses the existing **`play`** permission and the destination household's
linked Spotify renderer. Concrete track and episode identities do not require
a Spotify Web API lookup or Web API sign-in. Duplicate URIs and mixed tracks
and episodes are intentional occurrences, not a request to expand containers.
Room aliases must resolve to one room; an explicit ordered selection targets
one existing group and one destination queue revision.

### Add without changing playback

`POST /queue/tracks` accepts:

```json
{
  "zone": "Kitchen@S2",
  "tracks": [
    {"source_uri": "spotify:track:0000000000000000000001", "title": "First movement"},
    {"source_uri": "spotify:track:0000000000000000000002", "title": "Second movement"},
    {"source_uri": "spotify:track:0000000000000000000001", "title": "Repeat first movement"}
  ],
  "next": true,
  "expected_revision": "copy-the-opaque-revision-from-get_queue"
}
```

The numeric service ids above illustrate the request shape; use real playable
Spotify sources. `zone` and `tracks` are required. `next` defaults to false;
per-track `title` and `expected_revision` are optional. Spotify track and
episode links normalize to canonical URIs. Containers, unsupported sources,
malformed ids, an empty selection, more than ten entries, and unknown JSON fields
are rejected before speaker writes. Titles are cosmetic and use the existing
play-request normalization: trim, omit empty titles, and retain at most 256
characters.

Read `/zones/{room}/queue?offset=0&limit=1` to obtain the destination's opaque
`revision`. This is the Sonos queue revision, **not the cached library revision**.
When supplied, a different queue revision or coordinator produces
`QUEUE_CHANGED` before any insertion. Omitting it deliberately acts on the
freshly observed current queue.

The answer is the existing `QueueAddedDto`: `done`, coordinator room `zone`,
1-based `first`, exact inserted `count`, and `playing_queue`. A success confirms
the complete contiguous range. The server resolves member room names to their
current coordinator and uses the existing `add_to_queue` permission; there is
no separate policy grant that bypasses queue controls.

MCP exposes **`add_tracks_to_queue`** in the Full tool profile with the same
`zone`, ordered `tracks`, `next` and `expected_revision` fields. It delegates to
the same surface and permission. HTTP's OpenAPI operation has the same name.

## CLI

```sh
fsonos play-tracks 'Kitchen@S2' \
  spotify:track:0000000000000000000001 \
  spotify:episode:0000000000000000000002 \
  spotify:track:0000000000000000000001 \
  --expected-revision 'revision-from-fsonos-queue'

fsonos queue-tracks 'Kitchen@S2' \
  spotify:track:0000000000000000000001 \
  spotify:episode:0000000000000000000002 \
  spotify:track:0000000000000000000001 \
  --next --expected-revision 'revision-from-fsonos-queue'
```

CLI arguments retain their order and duplicates. `play-tracks` starts the
selection; `queue-tracks` adds it quietly. Omit `--next` on `queue-tracks` to append.
`--json`, `--daemon`, `--daemon-url` and `--direct` use the usual CLI routing and
output rules. A daemon request is one native batch request; a failed response
does not fall back to repeated single-source requests or local playback.

## Failure and undo

### Replacing playback

Play-now retains the original group, boot, source and queue before renderer
learning or optional revision checks. Preparation failures leave playback and
levels untouched. The existing replacement engine stages and verifies the
complete selected suffix before removing the old prefix with its queue revision
guard, then selects the first occurrence and starts it in `NORMAL` order.
Playback caps apply to the complete affected group.

A fully verified replacement of a nonempty, actively selected own queue can
retain its complete before/after receipt for one undo. Starting from an empty
queue or a direct source remains supported, but leaves the existing undo
barrier when no complete original queue proof is available. A failed or partial
replacement is not automatically replayed or described as fully reversible.
Inspect the actual queue and action log before deciding on another request.

### Adding to the current queue

The server reuses the existing bounded core insertion. It verifies the exact
inserted range as it proceeds and does not start, pause, seek, replace the queue,
or expand service containers. It supports S1 and S2 through their native queue
controls and learned Spotify renderer context.

An individual lost acknowledgement can be confirmed from the speaker queue
without resending that add. If the whole range cannot be established, the
request reports `OUTCOME_UNCERTAIN` and stops. A partial selection is not atomic
and is not automatically removed or replayed. Inspect the current queue before
deciding what to do next. The action log places an undo barrier after an
uncertain queue edit so an older action cannot be mistaken for this selection.

A complete confirmed selection can retain one queue-add undo record containing
every inserted renderer URI in order, the exact before/after queue revisions,
and known coordinator boot and group ownership. Undo removes the checked range
together; it does not split one selection into independent undo steps. A newer
queue containing identical tracks is still refused. Verified undo transitions
preserve successive undos across daemon restarts, while missing or changed
proof creates a barrier. See [Queue undo](QUEUE_UNDO.md#queue-additions) for
receipt bounds, counter requirements and legacy-action behavior.

## Regression checks

The shared browser contract, phone and dashboard models, actual checkbox
components, ordered review, late-read cancellation, source/group/queue changes,
offline selection, and lost HTTP response are exercised by:

```sh
node --test scripts/test-library-track-selection.cjs
cargo test --locked -p fsonos-api --lib surface::queue::tracks::tests
cargo test --locked -p fsonos-api --test queue_tracks_sim
cargo test --locked -p fsonos-api --test play_tracks_sim
cargo test --locked -p fsonos-cli --test e2e_play_tracks
```

The simulator tests cover S1/S2, duplicates and episodes, next/append, paused and
radio playback, one undo receipt, stale revisions, policy, lost add replies and
partly applied selections. CLI/MCP integration is covered by
`fsonos-cli --test e2e_queue_tracks` and their normal tool tests. Tests use
synthetic catalogs and simulator speakers; they do not prove current Spotify
account or physical-speaker availability.

Ordered playback adds S1/S2 cases for mixed tracks and episodes, duplicate
occurrences, all-member volume caps, complete replacement undo, direct-source
and empty-queue starts, changed destination evidence, lost acknowledgements and
partial staging. Browser Node tests execute the real selection components and
their request/receipt logic with synthetic observations. The new Rust checks
remain unexecuted behind the local `asupersync` dependency build failure; Node
coverage does not establish Rust or physical-speaker execution.
