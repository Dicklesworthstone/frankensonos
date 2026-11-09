# Saved Sonos playlists

FrankenSonos can list, inspect, play and create playlists saved on the Sonos household, including
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

## Save the current queue

`fsonos playlist-save <room> <title>` saves the current group's nonempty queue
as a new native playlist. It leaves playback, the queue, volume and the DJ alone.
A retained queue can also be saved while radio or another source is playing;
the saved content is the queue, not the active radio stream.

Titles are trimmed, must be nonblank and contain no control characters, and
may contain at most **1023 UTF-8 bytes** after trimming. Saving the same title
again creates a distinct `SQ:` id. Existing playlists are never overwritten;
use the returned id to distinguish equal titles. Editing, renaming and deleting
saved playlists are not exposed, and creating one is not an action-history undo
operation.

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
| Save the current queue as a new playlist | `POST /sonos/playlists/save` with `{"zone":"Kitchen","title":"Evening Mix"}` | `save_sonos_playlist(zone="Kitchen", title="Evening Mix")` |
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
27 new cases and the existing source/contents suites pass together (164 tests).
The corresponding core/API/CLI/MCP Rust regressions are added but have not run
past the dependency-build resource limit described above.
