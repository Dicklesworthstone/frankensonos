# Read the cached contents of a playlist

`GET /library/playlist` opens one playlist's last cached observation. It works
without speaker discovery, a running DJ, or a Spotify request. It uses the
existing `browse_library` read permission. Browsing never refreshes Spotify or
changes playback; `POST /dj/sync` remains the explicit refresh operation.

## Query and continuation

| Field | Meaning |
|---|---|
| `playlist_uri` | Required canonical `spotify:playlist:<id>` from the cached playlist listing. |
| `offset` | Zero-based index in the filtered cached occurrences; default `0`. |
| `limit` | Maximum occurrences returned, from `1` to `50`; default `50`. |
| `q` | Optional title, artist, or album words, at most 512 characters. Every normalized word must match. |
| `expected_revision` | The preceding page's local observation revision. Required whenever `offset` is greater than zero. |

Continue using `next_offset`, the returned `revision`, and the same playlist and
filter. A changed revision fails with `INVALID_ARGUMENT` and a restart hint.
Restart at offset zero without the old revision. There is no retained server
snapshot or automatic walk through all pages.

Unknown, repeated, or invalid query fields fail before the store read. An
unknown playlist is `NO_MATCH`. No local store is `NOT_READY`. A playlist whose
name is cached but whose contents have never been read is a successful
`not_cached` response, not a fabricated empty playlist.

## Occurrences and coverage

Each item carries an exact `source_uri`, title, subtitle, album metadata,
duration, explicit-content flag, and its original **one-based playlist
position**. Repeated tracks remain separate occurrences. Unavailable entries,
local files, episodes, and unsupported tracks occupy source positions but are
not exposed as playable rows. If positions 2 and 4 were unavailable, returned
positions can be 1, 3, and 5; they are never renumbered.

These are playlist positions, not Sonos queue positions. Play or queue the
selected item's exact `source_uri` through the existing `/play` or `/queue`
operation. Do not pass a playlist position to `queue-play`.

The page distinguishes these counts:

| Field | Meaning |
|---|---|
| `source_total` | Spotify's advertised number of source entries, including unsupported entries; `null` if not supplied. |
| `examined` | Raw source entries actually examined. |
| `cached_total` | Playable occurrences in the observation, before the text filter. |
| `total` | Cached occurrences matching `q`. |
| `fetched_at` | Unix seconds when this observation was read; `null` when no observation exists. |

`coverage` is `complete`, `partial`, or `not_cached`. Complete means the bounded
pagination reached its end without a read error. It does **not** mean an atomic
Spotify snapshot or that the playlist is still unchanged. A complete playlist
can have no playable rows because all source entries were unavailable. A
complete observation with `examined: 0` records an observed empty playlist.

`revision` identifies the entire local observation, including its timestamp,
coverage, occurrence positions, and metadata. It is not a Spotify snapshot ID,
an authorization token, or a playback reservation. The response's `q` echoes
the requested filter with surrounding whitespace removed.

## How synchronization records contents

The existing authorized taste sync reads the owner's own and collaborative
playlists. It now preserves ordered contents during those same reads, alongside
the deduplicated library used for search and DJ selection. This adds no Spotify
requests and grants no new scope.

The existing budget is at most 25 eligible playlists and four pages of 50 raw
entries per playlist. A longer playlist therefore produces an explicitly
partial observation. The URI/name listing can contain other playlists whose
contents were not eligible or within this budget; those remain `not_cached`.

Each observation is replaced atomically in the local store. A failed refresh
keeps the previous observation, including its original timestamp and revision.
When no observation exists, a valid prefix can still be retained as partial.
A successful read that reaches its configured budget can replace the old cache
with a new partial observation. A new prefix is never joined to an older suffix.

Migration 14 adds `spotify_playlist_contents`, keyed by playlist URI, with one
serialized observation per row. This keeps duplicate occurrences and their
metadata separate from the general track cache. The in-memory and SQLite stores
use the same validation and revision calculation.

## CLI and agents

List the cached playlist names and URIs, then open one:

```bash
fsonos dj browse playlists
fsonos dj browse tracks --playlist-uri 'spotify:playlist:<id>'
fsonos dj browse tracks --playlist-uri 'spotify:playlist:<id>' --q 'piano trio' --json
```

`--playlist-uri` requires the `tracks` category and cannot be combined with
`--album-uri`. For a later page, retain the playlist and filter and pass the
returned `next_offset` and `revision`:

```bash
fsonos dj browse tracks --playlist-uri 'spotify:playlist:<id>' \
  --offset 50 --expected-revision '<revision>' --json
```

The CLI reads the daemon's cache when connected and the local store in direct
mode. A daemon error does not silently switch to a different cache.

The full MCP profile exposes the read-only, idempotent `playlist_contents` tool:

```json
{
  "playlist_uri": "spotify:playlist:<id>",
  "q": "piano trio",
  "offset": 0,
  "limit": 50
}
```

Its arguments, result and revision rules match the HTTP endpoint. It uses the
existing `browse_library` policy permission. Select an item's exact
`source_uri` for `play` or `add_to_queue`; its playlist position is never an
argument to `play_queue`.

## Web remotes

In either web remote, open **Find music**, browse playlists, then choose
**View cached tracks**. The playlist's name filter is cleared so it does not
hide its tracks. Filtering within the playlist keeps its original occurrence
positions. **Back to playlists** returns to the playlist listing.

Each page explains its observed coverage and read time. A known playlist with
no recorded contents has a distinct not-yet-cached message. A complete empty
observation and a complete observation containing only unavailable entries
have different explanations. Explicit tracks remain visible.

Play now, Add to end and Queue next use the existing review and confirmation
flow. Review shows the source URI, original playlist position and destination
group. Confirmation checks the current group and rereads the selected cached
page with its expected revision, verifying the exact occurrence before sending
the track URI to the existing playback route. A changed observation requires
a fresh review. This reread checks the local cache; it does not contact Spotify
or reserve playback. An uncertain write is not retried automatically.
