# Ordered tracks and episodes in scenes

A scene can retain a short, exact listening sequence along with its room groups,
volumes and mute settings. Use source `kind: "tracks"` with **1–10 Spotify tracks
or episodes**. Order, repeated occurrences and optional titles are retained.
Applying the scene replaces each specified group's queue with that sequence.
It can start playback or load the queue silently, and the same scene can run
through an ordinary **Apply scene** routine.

These selections store concrete music identities. They do not require a Spotify
Web API sign-in or a fresh catalog lookup on recall. Each destination household
still needs its own linked Spotify renderer and access to the selected music.
The scene does not store another household's account or replay metadata.

## Save, inspect and apply from the CLI

Write a scene such as `evening.toml`:

```toml
[[groups]]
coordinator = "Kitchen@S2"
members = ["Office@S2"]
playing = false
source = { kind = "tracks", tracks = [
  { source_uri = "spotify:track:0000000000000000000001", title = "Opening" },
  { source_uri = "spotify:episode:0000000000000000000002", title = "Interlude" },
  { source_uri = "spotify:track:0000000000000000000001", title = "Reprise" }
] }

[volumes]
"Kitchen@S2" = 20
"Office@S2" = 15
```

The numeric IDs illustrate the request shape; replace them with real Spotify
track or episode URIs or full links. Optional titles are display metadata and
do not select different music. Full Spotify links normalize to canonical URIs.
Repeated URIs remain separate occurrences, including their separate titles.

```sh
fsonos scene save evening --from evening.toml
fsonos scene show evening
fsonos scene apply evening
fsonos schedule add "weekdays 20:30" scene apply evening
```

Saving reads the available rooms and stores the definition without changing
speakers. Applying the scene can regroup rooms and change their levels before
loading the selected music. All members of a group must belong to its
coordinator's household. Separate groups can have different ordered selections;
each uses that household's renderer. Separate groups start independently.

`playing = false` pauses any previous source and loads the new queue without
ever sending `Play` for it. `playing = true` starts the first occurrence in
normal order. If `playing` is omitted, recall uses the destination's playback
intent observed during planning. Explicit intent is easier to review for a
scheduled routine. Each recall reloads the saved sequence, even when a previous
recall left that queue selected.

## Author and review in the web remote

Open **Scenes**, create a custom scene, choose its group, then choose
**Ordered tracks or episodes** as the source. Enter one track or episode URI
or full Spotify link per line. Add `|` followed by an optional title:

```text
spotify:track:0000000000000000000001 | Opening
spotify:episode:0000000000000000000002 | Interlude
spotify:track:0000000000000000000001 | Reprise
```

Choose **Playing** or **Paused** for recall, then review and save. The review
includes every occurrence in order, its complete URI, group membership and
playback intent. **Paused** means loading without starting playback. Saving
does not apply the scene. Invalid or oversized selections are rejected before
the save request; duplicates are never silently removed.

The phone remote and Listening Room display these saved scenes, review them
before recall, and accept them in routine creation and editing. The routine
review includes the full selection, so a changed order, title, URI or playback
intent requires a new review. Routine creation reads the saved definition;
speaker readiness is checked when the routine runs. Later deliberate edits to
the saved scene change what a future occurrence will apply.

The iPhone, iPad and Mac client also reads these saved scenes and shows every
occurrence in its shared scene, recall and routine reviews. Changes to an exact
URI, title or order invalidate an existing review. Use the web editor, CLI,
HTTP or MCP to author or change an ordered selection; the native custom scene
composer refuses unsupported selection fields.

## HTTP and MCP

Send an authored definition to `PUT /scenes/evening`:

```json
{
  "groups": [{
    "coordinator": "Kitchen@S2",
    "members": [],
    "source": {
      "kind": "tracks",
      "tracks": [
        {"source_uri": "spotify:track:0000000000000000000001", "title": "Opening"},
        {"source_uri": "spotify:episode:0000000000000000000002"},
        {"source_uri": "spotify:track:0000000000000000000001", "title": "Reprise"}
      ]
    },
    "playing": false
  }],
  "volumes": {"Kitchen@S2": 20}
}
```

For MCP, call `save_scene` with `name: "evening"` and the same object in `scene`.
Read it using `get_scene` or `list_scenes`. Apply using
`POST /scenes/evening/apply` with `{}`, or MCP `apply_scene` with that name.
The saved `tracks` array remains visible in scene responses, including order,
duplicates and normalized optional titles. An incomplete MCP apply sets the
tool-error flag and retains its complete receipt; inspect successful and failed
steps before another action.

Use the existing schedule contract for routine execution:

```json
{"when":"weekdays 20:30","action":"apply_scene","scene":"evening"}
```

Albums, playlists, artist/show containers, arbitrary streams, malformed IDs,
missing or empty selections, and more than ten occurrences are invalid in
`tracks`. Use the appropriate other scene source for a whole container or
stream. A tracks source cannot also name a favorite, URI, DJ, native playlist
or captured queue. Titles use the same normalization as immediate ordered
playback: trim, omit empty titles and retain at most 256 characters.

## Readiness, permissions and recovery

All permitted selections are prepared before scene grouping, cap lowering or
other writes. Preparation retains each destination's queue, source, player
identity and boot observation, verifies the household renderer, and checks the
intended audience. After intentional regrouping, the engine brackets the new
destination snapshot with full audience checks. An unrelated room joining
during that read cannot silently become part of the scene's playback audience.
The existing bound writer continues checking the destination during loading.

Both the caller and saved scene writer need the existing `play` permission for
these sources. Silent loading also requires `pause`. Their volume caps apply
to the intended group, including members the scene will join. Independent,
permitted room settings retain the scene engine's normal policy behavior.

The existing ordered writer stages and verifies the new entries before
removing the old prefix with a queue revision guard. It does not clear the old
queue first. Missing readiness leaves the house untouched. A later write
failure can leave the old source paused, staged entries appended, or other
groups already completed. A scene is not an atomic speaker transaction.
Inspect `complete`, `failed`, `skipped` and notes; uncertain writes are never
automatically replayed.

Scheduled readiness failures retry within the scheduler's existing grace
period before an occurrence is claimed. Once dispatch begins, an incomplete
or lost result does not replay the occurrence.

An eligible complete recall on unchanged groups retains each successful queue
replacement receipt and a bounded backup of the original queue. Reviewed undo
can reconstruct its exact occurrences, position, mode, play intent and the
scene's scoped volume/mute changes. Both playing and silent recall qualify.
Every replacement must have sufficient proof. Partial scenes, regrouping, DJ
changes, changed destinations and missing backups leave the whole action as an
undo barrier. A later listener change invalidates recovery. Inspect the undo
preview instead of assuming a successful apply guarantees undo; see
[Queue undo](QUEUE_UNDO.md).

An authored sequence differs from a [captured queue](SCENE_QUEUES.md): it keeps
a deliberately chosen, bounded set of Spotify identities and loads from its
beginning. Queue bookmarks retain an occurrence and time in an unchanged
original queue; durable backups retain complete captured contents and original
player identity. A [native saved playlist scene](SONOS_PLAYLIST_SCENES.md)
reloads that playlist's current contents.
