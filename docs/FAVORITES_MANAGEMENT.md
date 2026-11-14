# Create and manage My Sonos favorites

FrankenSonos can save the selected track or station as a native **My Sonos**
favorite, rename an existing favorite, and remove an exact favorite from its
household. The speaker supplies the playback URI and service metadata. Playback,
the retained queue, volume and DJ ownership continue unchanged.

A favorite belongs to its Sonos household. FV:2/7 in S1 and FV:2/7 in S2 are
different entries. Use a qualified room such as Den@S1 when room names repeat.
Catalog authoring addresses one existing group and preserves that household's
native source metadata.

## Save the current track or station

First preview what will be saved:

~~~sh
fsonos favorite-current 'Den@S1' --json
~~~

The result includes zone, a favorite with its title and kind, and an opaque
revision. Its favorite.id is empty because nothing has been created yet.
Copy the exact returned revision into the save:

~~~sh
fsonos favorite-save 'Den@S1' 'Evening radio' \
  --expected-revision '<revision from favorite-current>'
~~~

When the group uses its queue, this saves the **selected track occurrence**,
retaining its original native item identity, resource and service metadata.
Use playlist-save to save the complete queue. When the group plays a station
directly, the preview and save use the station's source metadata, so a transient
song title cannot replace the station. A direct native audio track is also
supported. A plain HTTP audio source without metadata can be saved as a stream;
review its source and provide the title you want.

Live inputs, TV routes, group-routing URLs and queue/container URLs are not
reusable tracks or stations and are refused. A source must already be selected
on the speaker. This feature does not create a music-service account or link
Spotify to a household, and needs no separate Spotify Web API lookup.

The save checks the same household, player, group, boot identity and source
again. Queue previews also retain the selected occurrence and queue revision.
Elapsed playback time may advance without invalidating the review. A changed
track, queue, catalog target or group requires a new preview before another
request. These observations cannot lock out an independent Sonos controller
between the final read and native write.

## Rename or remove a favorite

Read the existing favorites, then inspect the exact native ID:

~~~sh
fsonos favorites 'Den@S1'
fsonos favorite-inspect 'Den@S1' 'FV:2/7' --json
fsonos favorite-rename 'Den@S1' 'FV:2/7' 'Late-night radio' \
  --expected-revision '<revision from favorite-inspect>'
~~~

To remove an entry, inspect it again and pass its current revision:

~~~sh
fsonos favorite-remove 'Den@S1' 'FV:2/7' \
  --expected-revision '<revision from a fresh favorite-inspect>'
~~~

Names and numbered list positions cannot identify a rename or removal. Equal
titles remain separate native IDs. Rename preserves the source and service
metadata; a same-title request is a validated no-op. Removal deletes the saved
shortcut, leaving music already queued or playing intact. Catalog changes
cannot be undone through action history.

Titles are trimmed, must be nonempty, contain no Unicode control characters,
and fit 1023 UTF-8 bytes. Sonos can refuse an already-favorited source or a full
favorites catalog. A definite refusal is returned to the caller; it is not
converted into an invented successful save.

## HTTP and MCP

| Operation | HTTP | Full MCP profile |
| --- | --- | --- |
| Preview current source | GET /favorites/current?zone=Den%40S1 | current_favorite |
| Inspect exact favorite | GET /favorites/inspect?zone=Den%40S1&favorite=FV%3A2%2F7 | inspect_favorite |
| Save reviewed source | POST /favorites/save | save_favorite |
| Rename reviewed entry | POST /favorites/rename | rename_favorite |
| Remove reviewed entry | POST /favorites/remove | remove_favorite |

Save body:

~~~json
{
  "zone": "Den@S1",
  "title": "Evening radio",
  "expected_revision": "<revision from /favorites/current>"
}
~~~

Rename also requires favorite with the exact FV:2/<n> ID. Removal takes zone,
favorite and expected_revision, without a title. Caller-supplied renderer URIs,
service descriptors, DIDL, replacement modes and force flags are not accepted.
CLI and MCP use the selected daemon and never replay a failed remote write
against local speakers. Explicit CLI direct mode remains available.

Reads use list_favorites permission. A current-source preview additionally
requires get_zone_state. Writes use save_favorite, rename_favorite and
remove_favorite, all in the existing house-config policy class. Playback
permission alone does not authorize catalog changes.

## Confirmed and uncertain results

Each catalog mutation sends one native CreateObject, UpdateObject or
DestroyObject, then checks the original household's catalog. A successful
receipt contains done, zone, favorite, operation, changed and logged.
Removal returns the exact entry observed before deletion. logged: false
means the native catalog result was confirmed but the separate local action log
was unavailable; do not repeat that completed operation.

A network failure, malformed acknowledgement or failed postcondition after a
write returns OUTCOME_UNCERTAIN. The command is never automatically replayed
through address, coordinator or Spotify account recovery. Read the original
household's favorites before deciding on another change. A matching title alone
does not establish whether an unacknowledged save succeeded.

Native creation retains service descriptors and resource metadata, while storing
artwork at favorite level. Nested artwork is removed structurally from resMD
to avoid Sonos's documented metadata truncation behavior; other native extension
elements remain intact. The ContentDirectory encoding follows the primary
[SoCo favorites implementation](https://github.com/SoCo/SoCo/blob/master/soco/favorites.py).

## Validation

Protocol and S1/S2 simulator regression sources cover creation, rename, removal,
selected duplicate occurrences, direct stations, household-local IDs, catalog
and source changes, group/reboot changes, metadata preservation, native duplicate
refusals and lost replies. Surface tests cover permissions, action logging and
unchanged playback; MCP tests cover exact forwarding and no local replay; CLI
tests cover required revisions and input validation.

The pinned workspace formatter passed before the execution environment was lost.
The final files were then restored from retained source and patches through
GitHub and reviewed again. The restored tree could not be formatted or executed
locally. Rust compilation and runtime tests remain unverified: the existing
asupersync dependency build had already failed under memory constraints before
reaching project code, and the environment subsequently became unavailable.
Physical Sonos acceptance remains separate from simulator coverage.
