# Browse cached music

`GET /library/browse` reads the same local store as library search and the DJ.
It does not discover speakers, call Spotify, refresh the library, or start music.
It remains useful when speakers or Spotify are offline and does not require a
running DJ session. The daemon's normal browser checks and read authorization
apply (`browse_library` in an explicit client tool allowlist).

Query fields:

- `kind=tracks|albums|playlists` (default `tracks`).
- `offset=0` and `limit=50` (at most 50 entries per response).
- `q=` optionally filters title, artists and album; every word must match.
  Matching ignores case and the text normalizer's supported accents.
- `album_uri=spotify:album:<id>` with `kind=tracks` opens that album in disc/track
  order. A completed album cache supplies its full playable sequence; otherwise
  the response contains only the library's known tracks, with unknown numbers last.

Responses contain `items`, `total`, `offset`, `limit`, `next_offset`, `kind`,
`album_uri`, `revision` and `coverage: "cached"`. Alphabetical listings use the
source URI to break ties. Same-named albums and repeated titles stay distinct.
Each item has an explicit `source_uri`; playback must use that URI, not its
position or title. Existing `/play` and `/queue` retain all playback policy,
coordinator resolution, DJ ownership and action-log behavior.

The revision is an opaque listing change detector, not a credential or a
playback precondition. It covers the filtered listing's identities and displayed
metadata, not its page offset. For an album drill-down it also covers the completed
observation's fetch time and whether the response uses a complete album or a
library subset. A client moving between pages should restart
from the beginning when the revision changes, rather than splice two versions
of a listing together. There is no retained server snapshot. Store reads copy
only the requested category and release the store lock before filtering and
sorting. Response memory is bounded by the page limit; projection still reads
and sorts the selected cache category, as existing search does.

**Cached does not mean currently saved.** The store includes liked tracks,
saved-album tracks, other taste signals, and retired rows. A fresh Spotify read
may change that data. Explicit tracks remain visible; browsing is not DJ
selection. Albums are grouped by their service URI, and an album listing's
`cached_tracks` counts personal-library rows, not the complete Spotify album
length. Play/queue of an album still uses the album URI and the normal Sonos
container path.

An explicit album drill-down also reads that one album's completed catalog
cache, which the DJ's whole-work completion and background continuation already
populate. If it exists, its ordered occurrences supply the page, including
movements that were never liked or saved and repeated track URIs at distinct
disc/track positions. This does not add catalog tracks to the personal library,
global track browsing, search, or DJ candidacy. A completed observation remains
available while a replacement is unfinished. No network read is triggered.

Album pages add `album_observation`:

| Field | Meaning |
|---|---|
| `coverage` | `complete` for an accepted complete album cache, or `library_subset` when only library rows are available. |
| `fetched_at` | Unix seconds when the complete album list was read; `null` for a library subset. |
| `cached_total` | Playable cached occurrences before the text filter and pagination. |

Top-level `coverage` stays `"cached"` for compatibility. Complete describes the
last accepted playable sequence, not current Spotify availability or whether
the owner saved every track. Unsupported entries can leave gaps in disc/track
numbers. `total` still counts filtered occurrences. A fetch-time or content
replacement changes the revision, including when a library subset first becomes
a complete album or a changed occurrence is hidden by the current filter. Older daemons may omit the observation.

The completed cache supplies each occurrence's title, URI, disc/track position
and duration. Track-level artist and explicit metadata are reused only from an
exact URI match in the library; otherwise the artist is empty and
`explicit_known: false` marks the existing `explicit: false` placeholder as
unknown. An omitted `explicit_known` means true for older library responses.
Clients must not interpret an unknown explicit flag as a clean-content claim.
Album title comes from known album context when available. Position identity is
the disc/track pair for complete album pages; repeated URIs at different
positions remain selectable occurrences. Playlist membership is recorded separately during sync; open a
playlist's ordered occurrences through [`GET /library/playlist`](PLAYLIST_CONTENTS.md).

Invalid categories, oversized filters, unknown or duplicate query fields,
invalid album filters and limits outside 1–50 fail with `INVALID_ARGUMENT`.
No local store is `NOT_READY`, rather than a misleading empty library.
An empty cache is a successful empty page; refresh through `POST /dj/sync` is a
separate, explicit action.

## From the phone remote

Open **Find music**, then **Browse tracks**, **Browse albums**, or **Browse
playlists**. You can read the cache even before choosing a destination group.
The search box also filters a browse listing: enter words, then press Enter or
**Refresh listing**. Previous/Next page reads at most 50 items at a time.

**View cached tracks** opens the selected album's cached disc/track sequence;
it clears the album-title filter so that it does not hide movements. **Back to
albums** returns to the album listing. The page identifies complete versus
subset coverage, the unfiltered occurrence count, and when a complete list was
read. Catalog-only rows identify unavailable artist and explicit-content
metadata. CLI and MCP text output show the same coverage and a UTC read time. Choosing a destination
keeps the page you were looking at.

For a playlist, **View cached tracks** opens its observed track sequence and
**Back to playlists** returns to the listing. Original playlist positions and
duplicate tracks are preserved. The page shows when it was read and whether
coverage is complete, partial, or not yet cached. Unsupported source entries
leave gaps in the position numbers. Opening or filtering a playlist never
starts a Spotify refresh. Review identifies the selected occurrence and
rechecks that exact cached observation before sending its track URI.

Play now, Add to end and Queue next use the existing two-step review: inspect
the exact item and household-qualified destination, then confirm. The client
rechecks group membership before dispatch and sends the chosen source URI,
never a title search or a mutable page position. Album-track confirmation also
rereads the selected cached observation and verifies the same revision and
occurrence before dispatch. Changing category, filter,
page or destination consumes any previous review. Late replies cannot replace
a newer page. A changed cache revision during pagination asks for an explicit
refresh from the beginning; it never combines pages from two listings.

Closing the panel invalidates an unsent action. A failed or unreadable write
is not retried; read the room and queue before deliberately trying again.
The remote uses the same existing embedded library assets and public play/queue
routes, with no new automatic sync, microphone capture or playback polling.
