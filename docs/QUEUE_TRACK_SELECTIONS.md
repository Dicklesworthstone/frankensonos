# Play or queue an ordered selection of tracks

Choose a few movements from a cached album, several entries from a playlist,
or individual library tracks and play or queue them in one action.
The selection contains **1–10 tracks or episodes**, preserves their supplied
order and repeated occurrences. **Play selected now** replaces each selected
group's queue and starts the first track in normal order. The phone remote,
Listening Room, CLI, HTTP and MCP can address one group, a saved room set or
every existing group, with the same selection in each. Queue-next and append
leave current playback alone. All three operations share the existing policy,
action history and verified undo boundaries.

The phone remote and Listening Room also offer **Save selection as scene** for
the chosen listening group. This opens a reviewed scene draft with the exact
order, titles, repeated occurrences and full group membership. Choose whether
recall starts playback or loads silently; saving leaves current music and
queues unchanged. The saved scene can then run through an ordinary Apply scene
routine. See [ordered-track scenes](ORDERED_TRACK_SCENES.md) for the phone editor,
dashboard dialog and the equivalent CLI/API definition.

Both browser baskets also offer **Add selection to Sonos playlist** for one
displayed group. Choose an existing native saved playlist, review the ordered
additions and its complete current contents, then confirm **Add selected tracks
to playlist**. Source-page checks and the destination's native contents revision
protect the reviewed selection. This catalog edit leaves the playing queue and
music unchanged and does not consume a playback undo. A partial or lost reply
keeps the original playlist available for an explicit contents read; additions
are never replayed automatically. CLI `playlist-add-tracks`, HTTP
`POST /sonos/playlists/add-tracks` and MCP `add_tracks_to_sonos_playlist` accept
the same 1–10 explicit Spotify track or episode occurrences. See
[direct saved-playlist additions](SONOS_PLAYLISTS.md#add-spotify-selections-directly)
for the request, household renderer requirement and recovery behavior.

## From the phone remote or Listening Room

Open **Find music** and browse cached tracks, or open an album or playlist with
**View cached tracks**. Mark **Select track** beside the wanted entries.
Both clients keep your selection as you browse different albums, playlists,
filtered track listings and their pages. Collections enter the selection in the
order you first choose from them; tracks within each listing initially keep
their catalog order, regardless of checkbox order. The ordered selection
above the results offers **Earlier/Later** (or **Move earlier/Move later**),
**Repeat track**, and **Remove selection**. Once you customize the order,
additional selections append to it. Repeats each count toward the ten-track
limit; removing one occurrence keeps the others. Unchecking a source row removes
all its selected copies. A disc/track position or original playlist position
distinguishes separate source occurrences, even when they share one URI.

Keep the chosen listening group, or change **Selected track destination** to
**Rooms or saved room set**. Enter a room, saved alias, `here`,
`all`/`everywhere`, `all@S1` or `all@S2` in **Rooms or room set for
selected tracks**. Changing this destination keeps the selected tracks, custom
order and repeats, and retires any earlier review.

Choose **Play selected now**, **Queue selected next** or **Add selected to end**.
The review shows the complete ordered list and source URIs, plus every affected
group's current source and queue effect. Play-now explicitly reviews replacing
each existing queue and starting the selected music. Confirm once to submit one
daemon request for the complete room set. Grouped rooms are deduplicated;
separate groups keep their membership and start independently.

Queue-next places the list after each group's current queue occurrence.
When a group is playing another source, it appends to that queue without
activating it. Paused playback stays paused, and queueing does not raise volume.
Confirmed insertions and failed destinations stay visible in a partial result;
an uncertain or missing reply is never automatically replayed.

Selection is limited to ten entries across cached listings, each browsed in
50-entry pages. The list names each track's original album, playlist or filter.
The same song chosen from separate listings remains two intentional occurrences.
You can review the selection while looking at a collection directory or editing
a new filter; only the retained, observed tracks enter that review. An active
read must finish first.

**Refresh listing**, **Clear selection**, or closing the chooser clears the list.
Returning to a selected listing whose revision, contents or coverage changed
also clears it with an explanation. An unrelated uncached playlist or failed
listing does not discard music already selected elsewhere. You can browse and mark cached tracks
without online speakers, but review requires fresh destinations and readable
queues. Both clients retain the basket while editing its room-set destination.
The Listening Room also keeps it editable after a failed review; a successful
review closes the chooser only after the review is ready.

Before showing the review and again at confirmation, the clients reread every
distinct selected source page and its exact occurrences. Each listing retains
its own revision and complete/partial cache evidence; different collections do
not need matching revisions. Repeated copies share one verification read per
page; at most ten source pages are read in each pass.
Confirmation also checks every destination group, playback source and queue,
plus the complete resolved room-set revision. A changed listing, group, source,
queue or audience asks for another review. Queue-next also checks
that the current queue occurrence has not changed. These checks are optimistic
observations; they do not reserve speakers against another controller. The
server additionally retains and verifies its own destination through insertion
or staged replacement. Play-now also checks the reviewed current occurrence.

Only the selected cached occurrences are submitted. This does not fetch missing
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
For playback, `zone` accepts a room, saved alias, `here`, `all`/`everywhere`,
`all@S1` or `all@S2`. Rooms already sharing a coordinator are deduplicated.
Each selected group receives the same ordered occurrences and learns its own
household's renderer metadata. Existing groups keep their members and start
independently; this does not synchronize separate groups.

A multi-group response includes the same per-target `batch` receipt as
[other room-set playback](MULTI_ROOM_PLAYBACK.md#failures-policy-and-undo).
Each target keeps its household, coordinator, room and succeeded/failed/not-attempted
result. MCP marks partial delivery as an error while retaining every receipt.

`expected_revision` is a **single destination's queue token**. It is accepted
only when the selector resolves to one distinct group, including an alias
naming several members of that same group. Supplying one token for multiple
groups is rejected before speaker mutation. Without this caller token,
the server still retains and rechecks each original queue, source and group.

Both direct track endpoints accept the same room-set selectors. For reviewed
room-set operations, use `POST /sources/control` with
`source: {"kind":"tracks","tracks":[...]}` and the audience revision returned
by `GET /rooms/resolve`. Both browser baskets use this contract for room sets;
they also reread every source page and destination queue before confirmation.
The audience revision and a direct track endpoint's queue revision are
different tokens; see [reviewed source requests](MULTI_ROOM_PLAYBACK.md#reviewed-source-requests).

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

For one distinct group, the answer keeps the existing `QueueAddedDto`:
`done`, coordinator room `zone`, 1-based `first`, exact inserted `count`, and
`playing_queue`. An alias naming several members of that group receives the
same response.

For multiple groups, `zone` accepts a saved alias, `all`/`everywhere`,
`all@S1` or `all@S2`. Each group receives the complete selection once.
The response is the existing room-set queue receipt: `complete`, `added`
with each household/coordinator and exact inserted range, `failed` with each
target's error, `logged`, and `undoable`. All-failed batches also retain
every target. There is no aggregate queue position because groups own separate
queues. A scalar `expected_revision` requires one distinct group.

The server retains every destination before renderer learning and checks the
complete set before additions. Next inserts after each group's current queue
occurrence; a group currently using radio or another source receives an append
without activating its queue. The existing `add_to_queue` permission applies
throughout.

MCP exposes **`add_tracks_to_queue`** in the Full tool profile with the same
`zone`, ordered `tracks`, `next` and `expected_revision` fields. It delegates to
the same surface and permission. HTTP's OpenAPI operation has the same name.
MCP retains every group receipt and sets its tool-error flag for incomplete
delivery, including definite refusals and uncertain insertions.

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

For the same three occurrences in every S2 group, use:

```sh
fsonos play-tracks all@S2 \
  spotify:track:0000000000000000000001 \
  spotify:episode:0000000000000000000002 \
  spotify:track:0000000000000000000001 --json
```

Replace `all@S2` with a saved alias or `everywhere` as needed. The illustrative
Spotify identities above must be replaced with playable tracks or episodes.

CLI arguments retain their order and duplicates. `play-tracks` starts the
selection; `queue-tracks` adds it quietly. Omit `--next` on `queue-tracks` to append.
`--json`, `--daemon`, `--daemon-url` and `--direct` use the usual CLI routing and
output rules. A daemon request is one native batch request; a failed response
does not fall back to repeated single-source requests or local playback.
Partial multi-group delivery from either command exits unsuccessfully and
keeps the complete receipt in output, including under `--json`; inspect failed
targets before issuing another command. To add the same reviewed list quietly
to every group, use `queue-tracks everywhere <URI>... --next` or omit `--next`
to append.

## Failure and undo

### Replacing playback

Play-now retains the original group, boot, source and queue before renderer
learning or optional revision checks. For a room set, all destinations and
household renderer prerequisites are checked before any group's caps or
playback writes. Preparation failures leave playback and levels untouched. The existing replacement engine stages and verifies the
complete selected suffix before removing the old prefix with its queue revision
guard, then selects the first occurrence and starts it in `NORMAL` order.
Playback caps apply to the complete affected group.

A fully verified replacement of a nonempty, actively selected own queue can
retain its complete before/after receipt for one undo. Starting from an empty
queue or a direct source remains supported, but leaves the existing undo
barrier when no complete original queue proof is available. A failed or partial
replacement is not automatically replayed or described as fully reversible.
A complete room-set replacement records one action and offers one undo only
when every affected group has complete replacement proof; a partial batch
creates an undo barrier rather than silently omitting failed groups.
Inspect the actual queues and action log before deciding on another request.

### Adding to the current queue

The server reuses the existing bounded core insertion. It verifies the exact
inserted range as it proceeds and does not start, pause, seek, replace the queue,
or expand service containers. Room sets share the existing aggregate executor
and one action log. Definite refusals retain other groups' confirmed ranges;
one undo can remove those proved additions together. Any uncertain effect
creates a whole-action undo barrier. It supports S1 and S2 through their native queue
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
node --test scripts/test-library-track-selection.cjs scripts/test-library-basket.cjs scripts/test-dashboard-library-basket.cjs
cargo test --locked -p fsonos-api --lib surface::queue::tracks::tests
cargo test --locked -p fsonos-api --test queue_tracks_sim
cargo test --locked -p fsonos-api --test play_tracks_sim
cargo test --locked -p fsonos-api --test source_targets_sim
cargo test --locked -p fsonos-api --test queue_targets_sim
cargo test --locked -p fsonos-cli --test e2e_queue_tracks
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
partial staging. Room-set regressions also cover alias deduplication, mixed
S1/S2 destinations, per-household renderer preparation, shared undo, scalar
revision refusal and per-target partial receipts. Browser Node tests execute the real selection components and
their request/receipt logic with synthetic observations, including three-page
catalogs, mixed collections and filters, custom order, repeats, exact source
positions, delayed reads and lost HTTP replies. Room-set cases include actual
destination controls, failed and cancelled review lifecycles, replacement-card
command reservations, per-group receipts and the long write deadline. The
focused nine-suite native Node run passed all 383 tests:

```sh
node --test --test-reporter=dot \
  scripts/test-library-basket.cjs \
  scripts/test-dashboard-library-basket.cjs \
  scripts/test-remote-library.cjs \
  scripts/test-source-targets.cjs \
  scripts/test-dashboard-source-targets.cjs \
  scripts/test-library-track-selection.cjs \
  scripts/test-dashboard-library-actions.cjs \
  scripts/test-remote-http.cjs \
  scripts/test-dashboard-http.cjs
```

The new Rust checks remain unverified in this session. Earlier local builds
were killed in `asupersync` and `zerocopy`; the later workspace has insufficient
space for another full build. Rust formatting was checked with the available
nightly formatter, which is newer than the repository's pinned toolchain.
Native Node coverage does not establish Rust, rendered Chromium or
physical-speaker execution.
