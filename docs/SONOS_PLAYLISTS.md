# Saved Sonos playlists

FrankenSonos can list, play and create playlists saved on the Sonos household, including
ones that are not in My Sonos favorites. These are the native `SQ:` saved queues,
separate from the Spotify account's cached playlists.

## CLI

```sh
fsonos playlists Kitchen
fsonos --json playlists Kitchen
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

The review shows the destination and the effect on its current queue. Before
sending, the controller reads the catalog and destination again; a renamed or
removed selection, regrouped destination, or changed queue requires a fresh
review. Switching households clears the selection even if both catalogs contain
the same `SQ:` id. The speakers supply the playlist's current saved contents
when the command runs. A failed or incomplete write reply is shown as uncertain
and is not automatically retried.

## HTTP and MCP

| Operation | HTTP | MCP |
|---|---|---|
| List the selected household's saved playlists | `GET /sonos/playlists?zone=Kitchen` | `list_sonos_playlists(zone="Kitchen")` |
| Save the current queue as a new playlist | `POST /sonos/playlists/save` with `{"zone":"Kitchen","title":"Evening Mix"}` | `save_sonos_playlist(zone="Kitchen", title="Evening Mix")` |
| Replace the queue and play | `POST /sonos/playlists/play` with `{"zone":"Kitchen","playlist":"SQ:7"}` | `play_sonos_playlist(zone="Kitchen", playlist="SQ:7")` |
| Append | `POST /queue` with `{"zone":"Kitchen","playlist":"SQ:7"}` | `add_to_queue(zone="Kitchen", playlist="SQ:7")` |
| Play next in the queue | `POST /queue` with `{"zone":"Kitchen","playlist":"SQ:7","next":true}` | `add_to_queue(zone="Kitchen", playlist="SQ:7", next=true)` |

Listings return `id`, `title`, and optional `art_uri`. A queue request accepts
exactly one of `playlist`, `favorite`, or `source_uri`. Playback and queue operations
take a catalog selection, not caller-supplied renderer metadata or a file URI.
They resolve the selection from the target household again before execution.

The existing permissions apply: `list_favorites` for listing, `play_favorite`
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

The protocol follows the native catalog and enqueue operations documented in
[SoCo's music-library implementation](https://github.com/SoCo/SoCo/blob/master/soco/music_library.py)
and [queue implementation](https://github.com/SoCo/SoCo/blob/master/soco/core.py).
Its [DIDL model](https://github.com/SoCo/SoCo/blob/master/soco/data_structures.py)
records the two playlist element forms; FrankenSonos keeps the form it reads.
