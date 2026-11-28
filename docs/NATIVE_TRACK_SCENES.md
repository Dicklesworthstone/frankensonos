# Exact native playlist selections in scenes and routines

A scene can retain **1–10 exact occurrences from a saved Sonos playlist**, including
local files, non-Spotify service tracks and mixed selections. Source
`kind: "sonos_playlist_tracks"` stores the selected order, repeated occurrences,
titles and complete speaker-owned playback metadata. Later edits to, renaming or
removal of the original playlist do not change the saved sequence.

This differs from `kind: "sonos_playlist"`, which loads the whole playlist's current
contents on every recall. Both sources are limited to their original Sonos
household. A native selection can play in another room or new scene group in
that household; it cannot copy one household's service metadata into another.

## Save a selection

Read the source household's saved playlist and its contents using
`fsonos playlist-contents Kitchen SQ:7`, HTTP
`GET /sonos/playlists/contents?zone=Kitchen&playlist=SQ:7`, or MCP's native
playlist contents read. Retain the returned **contents revision** and the original
one-based positions. The revision belongs to that observed source group and
playlist, independently of the destination scene group.

An authored scene file can contain:

```toml
[[groups]]
coordinator = "Office@S2"
members = ["Hall@S2"]
playing = false

[groups.source]
kind = "sonos_playlist_tracks"
name = "SQ:7"

[groups.source.selection]
zone = "Kitchen@S2"
positions = [3, 1, 3]
expected_revision = "COPY_THE_OBSERVED_PLAYLIST_CONTENTS_REVISION"

[volumes]
"Office@S2" = 20
"Hall@S2" = 15
```

Replace the example room names, playlist id and revision with the values read
from your house. The source `zone` is the room used to browse the playlist.
The scene coordinator and members describe where the saved selection will play.

```sh
fsonos scene save native-evening --from native-evening.toml
fsonos scene show native-evening
fsonos scene apply native-evening
fsonos schedule add "weekdays 20:30" scene apply native-evening
```

Saving performs speaker reads and writes the local scene definition. It does
not change playback, queues, grouping or volume. A stale contents revision,
missing or unplayable occurrence, or destination in another household refuses
the save. Every requested occurrence must be available; repetitions are not
deduplicated and unavailable entries are not skipped.

The source revision must still match the original group, player boot and
playlist observation during capture. If the source group changed after browsing,
read the contents again before saving. After saving, the exact copied occurrences
are independent of that catalog revision.

## HTTP and MCP

Send an authored definition to `PUT /scenes/native-evening`:

```json
{
  "groups": [{
    "coordinator": "Office@S2",
    "members": ["Hall@S2"],
    "playing": false,
    "source": {
      "kind": "sonos_playlist_tracks",
      "name": "SQ:7",
      "selection": {
        "zone": "Kitchen@S2",
        "positions": [3, 1, 3],
        "expected_revision": "COPY_THE_OBSERVED_PLAYLIST_CONTENTS_REVISION"
      }
    }
  }]
}
```

For MCP, pass the same definition as `scene` to `save_scene`, together with its
`name`. Existing scene replacement revisions still apply: use the saved scene's
`expected_revision` to replace only the reviewed definition. That scene revision
is separate from `selection.expected_revision`, which identifies the playlist
contents used for capture.

Reads return `kind`, the original playlist's `name` and `playlist` provenance
(household, native id and title), plus each saved track's `source_uri` and optional
`title` in exact order. Complete DIDL metadata and the capture admission request
are not exposed. As with other captured or household-bound sources, this saved
DTO is for display and review; it is not an authored source definition. To
replace a native selection, browse and submit a new reviewed selection.

## Browser authoring

Both browser remotes can create these scenes directly from the saved Sonos
playlist browser. Choose **1–10 tracks** across its pages, then use the selected
track controls to repeat, reorder or remove occurrences. **Save selection as
scene** carries that exact sequence into the scene review, including local files
and native service tracks. The preview retains full titles and resource strings;
renderer metadata stays on the daemon.

The entry point names the currently displayed listening group as the scene's
destination. Clear **Selected-track destination** before using it: that separate
playback field can name a room set, while scene creation starts from one
representable group. The full remote's custom scene editor then allows a
different coordinator and members in the same household. Changing those scene
rooms retains the original playlist reader and contents revision. The dashboard
saves the displayed group and shows its rooms in the review.

Name the scene, choose playing or silent loading, review the ordered occurrences
and destination, then confirm **Save**. Both clients reread every retained source
range and the original group before review and again before saving. A changed
playlist revision, source group or destination retires the review without a
write. Existing scene replacements retain their saved-scene revision check.
After a save is sent, an unreadable or mismatched receipt is reported as uncertain
and the write is not repeated. None of these authoring steps starts playback.

The public saved-scene DTO omits the original capture request and renderer
metadata, so the browser's general saved-scene editor does not turn a saved
native sequence back into an authored selection. To replace it, choose a fresh
playlist selection and save it under the existing scene name after reviewing
the replacement. Saved selections remain independently recallable after the
original playlist changes or disappears.

## Recall and scheduled use

`playing: true` replaces the destination queue, selects the first saved
occurrence and starts it in normal order, with shuffle and repeat off.
`playing: false` confirms the old source is quiet, loads the same queue and
selects its first occurrence without sending Play. Authored source definitions
default to playing when that field is omitted, matching other authored scenes.

Both browser remotes and the Apple client display the saved source, its original
household and every occurrence in order. They review and recall it, and accept
it in Apply scene routines. A changed resource, order, repetition, title,
household binding or saved definition invalidates an existing review. The CLI,
HTTP and MCP interfaces also support authoring these selections.

Recall does not require the original playlist, a Sonos favorite or Spotify Web
API sign-in. The speakers still need access to the underlying files or service
account. The destination must remain in the saved household, and playback
retains the scene executor's confirmed group and current volume caps.

The saved author and applying caller need the existing `play_favorite`
permission; silent loading also needs `pause`. Ordinary scene permission
filtering, group dependencies and scheduled occurrence behavior apply.

The shared native queue engine stages and verifies all occurrences before
removing the old queue prefix. Partial or uncertain writes are not replayed.
A complete verified scene on unchanged groups can retain bounded queue-content
and scoped-level undo. Regrouping, partial scenes and missing replacement proof
keep the existing undo barriers; inspect the result and undo preview.

Each saved selection is limited to 10 occurrences and 4 MiB of retained metadata.
Native resource strings are limited to 65,536 UTF-8 bytes and titles to 4,096
Unicode characters, with exact resource bytes and valid display whitespace
retained.

## Validation

The change adds real S1/S2 socket-simulator regressions for mixed local and
native service metadata, repeated occurrences, silent and audible recall,
stale source and cross-household refusal, regrouping and saved-author caps,
bounded queue undo, partial writes and scheduled recall without the original
catalog. Browser and Apple regressions cover the saved-source contract and
changed-review behavior.

Browser authoring regressions additionally exercise both clients' actual
selection handoffs, S1/S2 saves, repeated occurrences from distant pages, silent
loading, separate source and destination rooms, stale observations and uncertain
receipts.

Run the focused regression suites with:

```sh
cargo test -p fsonos-core --test native_track_batch_sim scenes::
cargo test -p fsonos-api --test scenes_sonos_playlists_sim native_selections::
node --test scripts/test-remote-scenes.cjs scripts/test-dashboard-session.cjs
node --test scripts/test-native-track-scene-authoring.cjs
swift test --package-path apps/apple --filter frozenNativeScenes
```
