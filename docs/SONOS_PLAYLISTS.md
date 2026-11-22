# Saved Sonos playlists

FrankenSonos can list, inspect, play, create, compose, rename, edit and remove playlists saved on the Sonos household, including
ones that are not in My Sonos favorites. These are the native `SQ:` saved queues,
separate from the Spotify account's cached playlists.

## CLI

```sh
fsonos playlists Kitchen
fsonos --json playlists Kitchen
fsonos playlist-contents Kitchen 'Evening Mix'
fsonos --json playlist-contents Kitchen SQ:7 --limit 25
fsonos playlist-save Kitchen 'Evening Mix'
fsonos play Kitchen --playlist 'Evening Mix'
fsonos play Kitchen --playlist SQ:7 --next
fsonos play Kitchen --playlist SQ:7 --queue
```

Choose the playlist's native `SQ:<n>` id, its current numbered position in the
listing, or an unambiguous title. Exact titles take precedence over prefixes and
word matches. Ambiguous matches return the candidates and change no playback.
The room selects the household; `Kitchen@S1` and `Kitchen@S2` distinguish repeated
room names. A playlist id is local to that household. `SQ:7` on S1 need not name
the same playlist as `SQ:7` on S2.

Plain `--playlist` replaces the group's current queue and starts its first
track. `--next` inserts the saved tracks after the current queue track; `--queue`
appends them. Those two additions leave the current source and playback alone
and use the existing confirmed queue-range undo. Repeated tracks retain their
separate occurrences. When the current source is not the queue, `--next` appends
without switching sources.

The commands use the daemon when available and also work in direct mode. They
need no Spotify Web API sign-in or learned Spotify favorite template. The
speakers still need access to whatever music services or local files the saved
playlist contains.

## Inspect the saved tracks

`fsonos playlist-contents <room> <playlist>` reads up to 100 ordered track
occurrences without replacing the playing queue or interrupting music. It works
for both S1 and S2, independently of Spotify Web API sign-in. Track positions,
titles, artists, albums, durations and artwork remain the speaker's observations;
missing metadata stays unknown. Repeated tracks occupy separate positions.

The response includes the resolved native `playlist`, `total`, `offset`,
`items`, an opaque `revision`, and `next_offset` when more tracks remain.
Continue using the returned native id and revision, for example:

```sh
fsonos playlist-contents Kitchen SQ:7 --offset 25 --limit 25 \
  --expected-revision '<revision from the preceding page>'
```

Offsets are zero-based; displayed track positions are one-based. Limits are
1–100. Short speaker pages continue at the actual next occurrence. A nonzero
offset requires the preceding revision, and any revision-bound request requires
the exact native `SQ:` id rather than a title or catalog number.

Each page uses bounded reads of the selected playlist's metadata and children,
with a second read and a topology check before returning. Continuations bind to
the household, coordinator, boot identity, group and saved-content observation.
A changed observation returns `QUEUE_CHANGED` (HTTP 409): read again from
offset zero without `expected_revision`. Failed or incomplete reads do not
become an empty playlist. An empty saved playlist is a successful observation
with `total: 0` and no continuation.

These checks detect inconsistent observations; they do not lock the playlist
against another controller or make all pages an atomic snapshot. Browsing does
not select a track or send a playback command. Whole-playlist play/next/append
still uses the speaker's current saved contents when that separate action runs.

## Play or queue one saved track

An inspected occurrence can be selected without loading its entire playlist:

```sh
fsonos playlist-contents Kitchen SQ:7
fsonos playlist-track Kitchen SQ:7 3 --expected-revision '<revision from that page>'
fsonos playlist-track Kitchen SQ:7 3 --expected-revision '<revision from that page>' --mode next
fsonos playlist-track Kitchen SQ:7 3 --expected-revision '<revision from that page>' --mode append
```

The exact native playlist id, original one-based position and observed revision
are mandatory. A repeated song remains a separate occurrence at each position.
Titles, catalog numbers, raw renderer URIs and caller-supplied metadata cannot
substitute for this selection. Positions are bounded to 100,000, matching the
native contents reader's maximum catalog size.

`play` starts just the selected track and retains the existing queue. It uses
the speaker's native metadata, applies the usual playback volume caps, and
hands playback away from an active DJ as an ordinary explicit source change
does. A single track plays to its end; it does not schedule the remainder of
the playlist. `next` inserts one occurrence immediately after the current queue
track; when the group is not using its queue, it appends. `append` always adds
at the end. Both queue modes keep playback, volume and the current source
unchanged. Their confirmed one-track insertion can be removed with the usual
action-history undo.

The server reads the selected occurrence again at its expected revision and
retains its native service resource and DIDL. A missing resource, stream,
container or unavailable entry is refused. The play path rechecks the selected
track and group around snapshot and policy reads, then sends each native
playback stage once. It never redirects an old selection to a new coordinator
after a refusal. Queue writes use the existing native-track insertion path,
which confirms the inserted occurrence and its neighbors before recording undo.
Changed saved contents or player identity require a fresh contents read. These
checks detect changes; they do not lock Sonos against other controllers.

Selected tracks retain the original XML item under its original DIDL root,
including inherited namespaces, root and resource attributes, additional
resources and service descriptors, and vendor extensions. Playback, queue
insertion and copying selected tracks into another saved playlist forward that
retained metadata instead of rebuilding it from display fields. Both source
reads compare the complete retained XML, so an extension-only change also
invalidates the selection. Retained selection evidence is bounded to 4 MiB;
repeated root wrappers and deliberate repeated occurrences count toward that
budget before their documents are constructed or copied. These checks preserve
the source metadata; each speaker and service still decides whether it can play
or store the resource.

HTTP uses `POST /sonos/playlists/track` with
`{"zone":"Kitchen","playlist":"SQ:7","position":3,"expected_revision":"…","mode":"next"}`.
All five fields are required; `mode` is exactly `play`, `next` or `append`.
MCP `sonos_playlist_track` takes those same fields. CLI defaults `--mode` to
`play` and uses the selected daemon when available, with explicit `--direct`
and `--daemon` support. Playback requires `play_favorite`; queue modes require
`add_to_queue`, checked before any native catalog reads.

The result retains `playlist`, `position`, `title` and `mode`, names the
coordinator room in `zone`, and includes `done`, `changed` and policy/logging
`notes`. Queue receipts additionally report `first`, `count: 1` and
`playing_queue`. A failed or uncertain write is never automatically repeated;
inspect current playback or the queue before issuing another action.

## Play or queue an ordered selection

Select 1–10 native occurrences together, in the order you want to hear them:

```sh
fsonos playlist-contents Kitchen SQ:7
fsonos playlist-tracks Kitchen SQ:7 3 1 3 --revision '<contents revision>' --mode play
fsonos playlist-tracks Kitchen SQ:7 3 1 3 --revision '<contents revision>' --mode next
fsonos playlist-tracks Kitchen SQ:7 3 1 3 --revision '<contents revision>' --mode append
```

The plural command requires an explicit mode. Repeated positions deliberately
add repeated occurrences, and equal resources at different positions remain
distinct. Selection uses native resources and DIDL, including local files and
non-Spotify services, without a Spotify account lookup. A stale, missing or
unplayable occurrence refuses the complete selection before insertion.

| Mode | Result |
|---|---|
| `play` | Replace the queue with the selected occurrences, switch to `NORMAL` order, and start the first selected track. Playback caps and the normal DJ handoff apply. |
| `next` | Insert the selection immediately after the current queue occurrence, or append when the group is not using its queue. Leave playback and volume unchanged. |
| `append` | Append the selection in supplied order. Leave playback and volume unchanged. |

Batch `play` stages and verifies the entire selected list before removing the
original queue prefix. A confirmed replacement uses the existing bounded
queue-replacement undo, including its original occurrence, position and mode.
A confirmed `next` or `append` has one exact-range undo record for the whole
selection. Partial or ambiguous writes are not replayed; their outcome must be
inspected before another command. The singular `playlist-track` command keeps
its existing direct-play behavior, which retains the current queue.

HTTP uses `POST /sonos/playlists/tracks`; MCP uses `sonos_playlist_tracks`:

```json
{
  "zone": "Kitchen",
  "playlist": "SQ:7",
  "positions": [3, 1, 3],
  "expected_revision": "<saved-playlist contents revision>",
  "expected_queue_revision": "<destination queue revision>",
  "mode": "play"
}
```

Only `expected_queue_revision` is optional. It binds the action to a previously
reviewed destination queue and is distinct from the mandatory saved-playlist
contents revision. CLI accepts it as `--queue-revision` (also
`--expected-queue-revision`); `--revision` also accepts `--expected-revision`.
Unknown fields and caller-supplied renderer metadata are rejected. The server
retains the original destination group, source, boot and queue through source
selection and rechecks the selected occurrences before writing.

The phone and dashboard let you select up to ten occurrences from the displayed
saved-playlist page. The ordered review offers **Play selected now**, **Play
selected next**, and **Add selected to queue**, rechecks both revisions, and
sends one batch request. Closing the panel or losing a reply does not replay
that request. Changing the page or saved-playlist observation clears obsolete
selections. Playback requires `play_favorite`; queue modes require `add_to_queue`.

The native iPhone, iPad and Mac remote offers the same ordered selection in
**Music → Sonos → Read tracks**. Its selection can span same-revision pages;
add, move, remove or repeat up to ten occurrences, then choose **Review selected
tracks**. A one-entry selection uses the same queue-replacement or queue-add
contract. Review and confirmation reread every selected page and the destination
queue before sending one revision-bound request. Refreshing, changing households,
catalog failure or suspending the app clears the selection. See
[Native Apple remotes](APPLE_REMOTE.md) for the review and recovery behavior.

## Save the current queue

`fsonos playlist-save <room> <title>` saves the current group's nonempty queue
as a new native playlist. It leaves playback, the queue, volume and the DJ alone.
A retained queue can also be saved while radio or another source is playing;
the saved content is the queue, not the active radio stream.

Titles are trimmed, must be nonblank and contain no control characters, and
may contain at most **1023 UTF-8 bytes** after trimming. Saving the same title
again creates a distinct `SQ:` id. Existing playlists are never overwritten;
use the returned id to distinguish equal titles. Creating a saved playlist is
not an action-history undo operation. Renaming and removal use the separate,
revision-bound operations below. Saved occurrences can also be removed or moved
within an existing playlist without loading it into the playing queue.

The native save has no queue-revision compare-and-set argument. Sonos copies
the queue when it handles the request; a prior queue read does not lock out
another controller or a DJ top-up. The controller checks the current group and
confirms the newly assigned id and title by reading the saved catalog afterward.
It does not claim a previewed track count as an atomic saved snapshot.

A successful response includes the confirmed `playlist`, the coordinator's
room as `zone`, a `done` message, and `logged`. If `logged` is false, the
playlist is still saved on Sonos; only the separate local action log failed.
Do not repeat the save to repair logging. A lost write reply or failed
confirmation returns `OUTCOME_UNCERTAIN` and is never retried automatically.
Read this household's saved catalog before deciding whether to save again.

## Rename or remove a saved playlist

Read the exact playlist first, then use its returned `revision`:

```sh
fsonos --json playlist-contents 'Kitchen@S1' SQ:7 --limit 1
fsonos playlist-rename 'Kitchen@S1' SQ:7 'Evening & Friends' \
  --expected-revision '<revision from that contents read>'

# Read again after renaming: a rename changes the observed revision.
fsonos --json playlist-contents 'Kitchen@S1' SQ:7 --limit 1
fsonos playlist-remove 'Kitchen@S1' SQ:7 \
  --expected-revision '<revision from the fresh contents read>'
```

Both commands require an exact household-local `SQ:<n>` identity and a nonempty
contents revision. Names and catalog positions are rejected. The room, group,
player boot identity, playlist metadata and saved-content revision must still
match a fresh observation. Stale observations return `QUEUE_CHANGED` before a
catalog write; read and review again. The title rules are the same as for saving.
A same-title rename still validates freshness and returns `changed: false`.

Rename sends one native ContentDirectory `UpdateObject` with the observed
current title and requested new title, retaining the playlist's native id and
tracks. Removal sends one `DestroyObject` for that id and permanently removes
the saved entry. Equal titles in other entries are unaffected. Both operations
leave already loaded queues, playback, volume and DJ ownership unchanged. A
fresh, complete catalog read must confirm the new title or the removed id's
absence before success is reported.

Sonos does not accept our opaque revision as an atomic catalog lock. Another
controller can still change the playlist between the final read and native
write. An explicit native refusal is returned as a refusal; a lost or malformed
acknowledgement, changed group after submission, or failed confirmation remains
`OUTCOME_UNCERTAIN`. The controller never retries the write, recreates a removed
playlist, or replaces the playing queue as a fallback. Inspect the **original
household's** saved catalog before deciding on another edit.

Results include the confirmed renamed entry or the observed removed entry in
`playlist`, along with `operation` (`rename` or `remove`), `changed`, `zone`,
`done`, and `logged`. A logging failure leaves a confirmed native result
successful with `logged: false`; repeating the operation cannot repair the log.
Neither operation is undoable through action history.

Saved scene references retain the native title they originally reviewed as
well as the id. After renaming, re-save scenes that reference that playlist.
Removing a referenced playlist causes those scenes and their routines to refuse
playback until the source is replaced; catalog edits do not silently rewrite
scene definitions. See [native playlists in scenes](SONOS_PLAYLIST_SCENES.md).

## Remove or reorder saved tracks

Read the playlist first, then use its exact household-local `SQ:` id and the
returned revision for one edit:

```sh
fsonos playlist-contents 'Kitchen@S1' SQ:7 --json

# Move original tracks 2–4 before original track 1, keeping their internal order.
fsonos playlist-move-tracks 'Kitchen@S1' SQ:7 2 --count 3 --before 1 \
  --expected-revision '<revision from the contents read>'

# Read again after an edit; positions and the revision have changed.
fsonos playlist-contents 'Kitchen@S1' SQ:7 --json
fsonos playlist-remove-tracks 'Kitchen@S1' SQ:7 5 --count 2 \
  --expected-revision '<fresh revision>'
```

`position` is the first one-based occurrence, and `count` is the number of
consecutive occurrences. CLI `--count` defaults to one. Move `--before` refers
to the **original** playlist: its track count plus one moves the block to the
end. A position inside the block or immediately after it is a validated no-op.
Repeated tracks remain separate occurrences. Removing all tracks retains the
empty named playlist.

These operations edit the saved playlist in place. Its id and title stay the
same; future recalls, including scenes, use its edited contents. Loaded queues,
current music, volume and DJ ownership continue unchanged. Saved-track edits
have no playback undo and do not displace an earlier playback undo.

An edit requires a complete observation of at most **1,000 saved entries and
4 MiB of retained metadata**. Both observations check metadata, native content
counter, household, coordinator, group and available boot evidence. One native
`ReorderTracksInSavedQueue` carries the observed nonzero update counter. A
complete read-back must confirm the resulting order, count and revision.
Unavailable tracks can be removed; they need no playable resource.

The success receipt includes the original `position`, `count`, move
`insert_before` when applicable, `previous_total`, `total`, the new `revision`,
`changed` and `logged`. A stale observation sends no edit. Lost, malformed or
contradictory acknowledgements and failed confirmation return
`OUTCOME_UNCERTAIN`; the edit is never automatically replayed. Read the original
saved playlist before deciding on a new operation. Native update counters and
the read-back checks do not lock out another controller after confirmation.

Both web remotes offer **Edit saved track** beside each inspected occurrence
and range fields under **Manage saved playlist → Edit saved tracks**. Review
shows every selected occurrence and the original destination track, then
rechecks them inside the shared command lane before one request. Changing
the range, playlist, household or catalog invalidates the review. Pending and
uncertain results retain their original household through navigation; recovery
observes that catalog and requires a new review rather than repeating the edit.

## Add selected tracks to an existing playlist

Copy 1–10 observed occurrences from a group's retained queue or another saved
playlist into an existing native playlist. The destination keeps its `SQ:` id,
title and existing order. Positions are one-based and remain in the supplied
order; `2,4,2` deliberately adds the second source occurrence twice.

Read both the source and destination before adding:

```sh
# Copy from the retained group queue, including while another source plays.
fsonos --json queue 'Kitchen@S1'
fsonos --json playlist-contents 'Kitchen@S1' SQ:7 --limit 1
fsonos playlist-append-tracks 'Kitchen@S1' SQ:7 --positions 2,4,2 \
  --source-revision '<queue revision>' \
  --expected-revision '<destination contents revision>'

# Copy selected occurrences from another native saved playlist.
fsonos --json playlist-contents 'Kitchen@S1' SQ:1
fsonos --json playlist-contents 'Kitchen@S1' SQ:7 --limit 1
fsonos playlist-append-tracks 'Kitchen@S1' SQ:7 --source-playlist SQ:1 \
  --positions 3,1,3 --source-revision '<SQ:1 contents revision>' \
  --expected-revision '<SQ:7 contents revision>'
```

`source_revision` and `expected_revision` bind different observations. Both
are required; read them again after a native catalog change. Source and
destination belong to the selected room's household. Copying from the
destination itself is supported: the original selected occurrences are frozen
before any additions. An empty destination remains the same named playlist.

The server resolves the source positions again, retains each native resource
and DIDL document, and validates the entire selection before the first write.
It accepts native music tracks, including local files and supported service
tracks, without a Spotify Web API lookup. Missing resources, streams,
containers and unavailable entries reject the selection. Raw media addresses
and caller-supplied metadata are not request fields.

The completed destination must fit the existing **1,000-track / 4 MiB** saved
playlist bounds. Each `AddURIToSavedQueue` uses the most recently confirmed
nonzero native update counter. A complete read-back must confirm the original
prefix and the appended track before the next addition is sent. The live queue,
playback source, position, mode, volume and DJ ownership remain unchanged.
This is a catalog edit with no playback undo record; it does not replace an
earlier playback undo.

The batch is **not atomic**. If a later addition fails, earlier additions can
remain saved. An uncertain response reports how many of the requested
additions were confirmed and stops the batch. Nothing is retried or rolled
back automatically. Read the **original destination playlist** before deciding
whether to add any remaining occurrences. A stale observation or definite
refusal before the first addition remains an ordinary refusal. Native counters
and read-back checks cannot prevent another controller from changing the
playlist after confirmation.

The success receipt includes `playlist`, `zone`, `revision`, `previous_total`,
`total`, `added`, the original `positions`, selected `titles`, `done` and
`logged`. Text output includes the confirmed destination revision. If logging
fails, the additions remain confirmed with `logged: false`; do not repeat them
to repair the log.

Both web remotes expose **Manage saved playlist → Add tracks to this playlist**.
Choose **Tracks from**, enter the track numbers in order, and choose **Review
additions**. The review reads and shows the source titles, repeated selections,
destination household and resulting track count. Confirmation rechecks both
observations before one batch request. **Read original playlist contents**
recovers an uncertain result by showing the current count and entries at the
original append boundary, even after switching rooms. It reports the current
observation without claiming how the earlier request finished.

HTTP uses `POST /sonos/playlists/append-tracks`; MCP uses
`append_sonos_playlist_tracks` in the Full profile:

```json
{
  "zone": "Kitchen@S1",
  "playlist": "SQ:7",
  "expected_revision": "<destination contents revision>",
  "source_playlist": "SQ:1",
  "source_revision": "<source contents revision>",
  "positions": [3, 1, 3]
}
```

Omit `source_playlist` to select the group queue and use its queue revision.
The operation requires `append_sonos_playlist_tracks` in the `house-config`
class. CLI uses the selected daemon when available, supports explicit
`--direct` / `--daemon`, and never retries an answered daemon failure locally.

## Phone remote and desktop dashboard

Open **Play a link or Sonos playlist** in the phone remote, or the source dialog
in the desktop dashboard. Choose the destination group, select **Saved Sonos
playlist**, and choose a playlist from that household. **Play now**, **Queue
next**, and **Queue at end** use the same native operations as the CLI.

Choose **Read tracks** to inspect the selection first. **Next tracks** and
**Previous tracks** retain its observed revision; **Refresh tracks** starts a
new observation. Both views keep duplicate tracks and their original positions.
Changing the selected group, household or playlist clears the preview, and a
late response from the old selection cannot replace it. Reads are explicit and
do not add background polling or contact the track's media URL.

Track rows offer separate play/next/append actions. Their review retains the
saved occurrence and revision, and rechecks the destination and selected track
before sending. An obsolete selection requires another explicit read. Playing
one row retains the queue; whole-playlist controls keep their existing behavior.

For a single group's saved playlist selection, **Manage saved playlist** offers
**Review rename** and **Review deletion**. Enter a **New playlist title** for a
rename. Review reads the selected native playlist and shows its household,
exact id and track count; it does not read or alter playback. Confirmation
rechecks the group and contents revision inside the existing command lane.
Deletion requires a separate **Delete saved playlist** confirmation that
explains permanence and the effect on saved scenes and routines. A successful
rename or removal clears the stale selection and keeps a receipt for its
original household.

Closing the editor or choosing another household does not discard a submitted
catalog result. A lost or malformed reply requires **Read original household
catalog** before another rename or removal. The recovery result names the
original `SQ:` id and its current title or absence; it does not claim to prove
how the earlier request finished. Catalog changes in that household retire an
in-flight recovery read. An unrelated household event cannot acknowledge or
cancel that recovery. Both views share the same management and review logic.

The review shows the destination and the effect on its current queue. Before
sending, the controller reads the catalog and destination again; a renamed or
removed selection, regrouped destination, or changed queue requires a fresh
review. Switching households clears the selection even if both catalogs contain
the same `SQ:` id. The speakers supply the playlist's current saved contents
when the command runs. A failed or incomplete write reply is shown as uncertain
and is not automatically retried.

In the desktop dashboard, open **Speaker queue → Save queue as playlist**.
Read the selected group's queue, enter a name, review its destination and entry
count, then confirm **Create Sonos playlist**. The group and queue revision are
checked again immediately before the native save. The confirmed receipt keeps
its original household and `SQ:` id; **Browse saved playlists** opens that
household's existing source selector for a separate playback review.

A lost or malformed save reply blocks further dashboard commands until an
explicit **Check saved playlists** reads the original household's catalog.
Switching rooms or refreshing playback cannot acknowledge that save. Matching
names are shown as catalog observations, since duplicate names cannot prove
which request created a playlist. Any subsequent save requires a new queue
read and review.

## HTTP and MCP

| Operation | HTTP | MCP |
|---|---|---|
| List the selected household's saved playlists | `GET /sonos/playlists?zone=Kitchen` | `list_sonos_playlists(zone="Kitchen")` |
| Inspect ordered saved tracks | `GET /sonos/playlists/contents?zone=Kitchen&playlist=SQ:7&offset=0&limit=100` | `sonos_playlist_contents(zone="Kitchen", playlist="SQ:7", offset=0, limit=100)` |
| Play or queue one observed track | `POST /sonos/playlists/track` with `zone`, exact `playlist`, `position`, `expected_revision`, and `mode` | `sonos_playlist_track` with the same fields |
| Play or queue 1–10 ordered occurrences | `POST /sonos/playlists/tracks` with `zone`, exact `playlist`, `positions`, `expected_revision`, explicit `mode`, and optional `expected_queue_revision` | `sonos_playlist_tracks` with the same fields |
| Save the current queue as a new playlist | `POST /sonos/playlists/save` with `{"zone":"Kitchen","title":"Evening Mix"}` | `save_sonos_playlist(zone="Kitchen", title="Evening Mix")` |
| Rename an exact observed playlist | `POST /sonos/playlists/rename` with `zone`, exact `playlist`, `expected_revision`, and `title` | `rename_sonos_playlist` with the same fields |
| Permanently remove an exact observed playlist | `POST /sonos/playlists/remove` with `zone`, exact `playlist`, and `expected_revision` | `remove_sonos_playlist` with the same fields |
| Remove consecutive saved occurrences | `POST /sonos/playlists/remove-tracks` with `zone`, exact `playlist`, `expected_revision`, `position`, and `count` | `remove_sonos_playlist_tracks` with the same fields |
| Reorder a saved block | `POST /sonos/playlists/move-tracks` with the same fields plus original `insert_before` | `move_sonos_playlist_tracks` with the same fields |
| Add selected occurrences to an existing saved playlist | `POST /sonos/playlists/append-tracks` with `zone`, exact destination `playlist`, `expected_revision`, `source_revision`, `positions`, and optional exact `source_playlist` | `append_sonos_playlist_tracks` with the same fields |
| Replace the queue and play | `POST /sonos/playlists/play` with `{"zone":"Kitchen","playlist":"SQ:7"}` | `play_sonos_playlist(zone="Kitchen", playlist="SQ:7")` |
| Append | `POST /queue` with `{"zone":"Kitchen","playlist":"SQ:7"}` | `add_to_queue(zone="Kitchen", playlist="SQ:7")` |
| Play next in the queue | `POST /queue` with `{"zone":"Kitchen","playlist":"SQ:7","next":true}` | `add_to_queue(zone="Kitchen", playlist="SQ:7", next=true)` |

Listings return `id`, `title`, and optional `art_uri`. A queue request accepts
exactly one of `playlist`, `favorite`, or `source_uri`. Playback and queue operations
take a catalog selection, not caller-supplied renderer metadata or a file URI.
They resolve the selection from the target household again before execution.

The existing permissions apply: `list_favorites` for listing and contents reads, `play_favorite`
for starting, and `add_to_queue` for append/next. Playback uses the same volume
caps, DJ handoff, action history, and coordinator resolution as native container
favorites. This keeps existing client allowlists effective. Saving requires
`save_sonos_playlist`, in the `house-config` class; it records the catalog change
without a DJ handoff, volume changes or a playback undo record. The save body
accepts only `zone` and `title`, never an existing playlist id or overwrite flag.
Rename and removal require their own `rename_sonos_playlist` and
`remove_sonos_playlist` permissions, also in `house-config`. Saved-track edits
require `remove_sonos_playlist_tracks`, `move_sonos_playlist_tracks`, or
`append_sonos_playlist_tracks`, likewise
in `house-config`. A read-only catalog
allowlist cannot perform these writes. All six catalog mutation operations are in the
MCP Full profile. Request bodies reject unknown fields, including overwrite or
force flags.

## Playback and failure behavior

The catalog is read through bounded, revision-consistent ContentDirectory
pagination. Malformed identities, duplicate ids, incomplete pages, or failed
reads do not become selectable partial catalogs. The controller preserves the
speaker's resource and DIDL metadata, including either the `item` or `container`
element Sonos can use for a playlist.

Replacement uses the existing staged queue path: append the saved playlist,
verify its expanded tracks and the original prefix, then remove that prefix
under the speaker's queue revision guard before starting the new queue. A failed
or uncertain add is never sent again automatically. An uncertain result can
leave new tracks appended; read the queue before trying again. Replacement is
not an atomic transaction with another controller, and action-history undo does
not reconstruct a queue that replacement removed.

Native playlists exposed as My Sonos favorites also use this container path.
Their DIDL playlist or music-album class identifies them even when their URI
uses a native saved-queue or local-library scheme. Explicit radio sources keep
their direct-stream behavior.

## Verification

Native metadata retention has exact XML fixtures in `fsonos-proto` and outgoing
SOAP capture in `fsonos-core/tests/native_track_metadata_sim.rs`. The S1/S2
simulator cases check individual and batch playback, retained-queue copies,
duplicate selections, same-counter changes confined to vendor extensions, and
aggregate metadata limits. They verify the metadata submitted to the speaker;
physical service playback and how each firmware stores vendor extensions remain
separate acceptance checks.

```sh
cargo test --locked -p fsonos-proto --lib native_metadata
cargo test --locked -p fsonos-proto --lib content::native
cargo test --locked -p fsonos-core --test native_track_metadata_sim
```

`fsonos-core/tests/sonos_playlists_sim.rs` exercises real loopback SOAP for both
S1 and S2: listing without favorites, native metadata variants, replacement,
duplicate occurrences, append/next, exact unqueue, and lost enqueue replies.
The surface tests cover routing, policy, and caller input. Browser harnesses
cover the phone and dashboard controls, household changes, stale observations,
and one-shot submission over loopback HTTP. These simulator checks do not
establish physical-speaker playback acceptance.

Save regressions were added in `fsonos-core/tests/sonos_playlist_save_sim.rs`,
`fsonos-api/tests/sonos_playlists_sim.rs` and the CLI/MCP tests for both
households, persisted replay, unchanged playback, permissions and uncertain
replies. They have not been executed in this session: local Rust builds stopped
while compiling `asupersync` with SIGKILL, before test execution. Physical-speaker
save acceptance remains separate.

The dashboard save model, scoped command store and panel have Node coverage in
`test-dashboard-playlist-save.cjs`, `test-dashboard-playlist-store.cjs` and
`test-dashboard-playlist-save-panel.cjs`. They exercise native JSON routes,
bounded queue reads, changed-group and queue fences, private household
identities, duplicate titles, uncertain replies, and explicit catalog recovery.

The protocol follows the native catalog and enqueue operations documented in
[SoCo's music-library implementation](https://github.com/SoCo/SoCo/blob/master/soco/music_library.py)
and [queue implementation](https://github.com/SoCo/SoCo/blob/master/soco/core.py).
Its [DIDL model](https://github.com/SoCo/SoCo/blob/master/soco/data_structures.py)
records the two playlist element forms; FrankenSonos keeps the form it reads.

Native catalog management is covered by
`fsonos-core/tests/sonos_playlist_management_sim.rs`, protocol wire tests and
the API/CLI/MCP native-playlist suites. They exercise S1/S2 identities, duplicate
names, unchanged queues and radio, stale revisions, group and boot changes,
refusals, lost replies and unconfirmed results. The protocol follows the
[ContentDirectory action definitions](https://sonos.svrooij.io/services/content-directory)
and the device-specific title handling documented in
[SoCo's metadata updater](https://github.com/SoCo/SoCo/blob/master/soco/favorites.py):
commas and backslashes remain literal inside XML-escaped title fragments.
Physical-speaker rename/removal acceptance remains unverified; an unsupported
native action is surfaced without an alternate destructive operation.

`scripts/test-sonos-playlist-management.cjs` covers both shipped browser models,
the explicit destructive review, unchanged-title receipts, revision and group
changes, command coordination, closed editors, late replies, logging failures,
and original-household catalog recovery. The same suite runs in Linux CI.

Contents-specific simulator coverage lives in
`fsonos-sim/tests/saved_playlist_contents.rs` and
`fsonos-core/tests/sonos_playlist_contents_sim.rs`; API, CLI and MCP regressions
extend the existing native-playlist suites. The phone and dashboard readers
share `scripts/test-sonos-playlist-contents.cjs`, which covers duplicate
occurrences, both paging directions, changed revisions, delayed replies,
household changes, empty playlists and text-only metadata rendering. Rust
execution remains blocked in this environment by a SIGKILL while compiling
the pinned `asupersync` dependency. Node contents and existing source-player
regressions pass; physical-speaker contents acceptance remains separate.

Selected-track browser coverage runs with
`node --test scripts/test-sonos-playlist-track-actions.cjs`. It exercises all
three modes on both shipped command models, exact duplicate positions, stale
catalog and destination observations, missing resources, changed households,
late reads, one-shot submissions, lost replies and mismatched receipts. The
track-action file now has 66 cases, including 34 additions for ordered batch
selection and both production DOM panels. The focused source, contents,
management, room-target and HTTP-budget suites pass together (422 tests).

Batch simulator coverage in `fsonos-core/tests/native_track_batch_sim.rs` and
the extended native selection and queue-replacement undo suites covers local
metadata, duplicate order, sparse pages, silent queue modes, stale proofs,
partial writes, lost replies and complete replacement undo. API, CLI and MCP
regressions cover mode permissions, request validation, action history,
once-only daemon delegation and typed receipts. These Rust additions have not
run: the baseline workspace check was killed during dependency compilation,
and the focused core check stalled in `syn` before reaching project code.
Physical-speaker batch acceptance remains unverified.

Saved-track edit regressions extend the same management suites and
`fsonos-core/tests/sonos_playlist_tracks_sim.rs`. They cover exact duplicate
occurrences, forward/backward block moves, unchanged-position moves, removing
all entries, stale or paged observations, native refusals and uncertain replies,
with live queues and playback retained. The focused browser management suite
passes all 52 cases; the final management, source and request-deadline run passes
all 166 cases. A temporary harness of the actual protocol source, omitting only
the runtime transport module, passes 127 protocol tests. The three exact-source
simulator ordering tests also pass. These partial checks do not establish full
daemon, storage or physical-speaker acceptance: the full workspace build was
killed while compiling the existing `asupersync` dependency.
