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
- `album_uri=spotify:album:<id>` with `kind=tracks` lists the cached tracks of
  that album in disc/track order, with unknown numbers placed last.

Responses contain `items`, `total`, `offset`, `limit`, `next_offset`, `kind`,
`album_uri`, `revision` and `coverage: "cached"`. Alphabetical listings use the
source URI to break ties. Same-named albums and repeated titles stay distinct.
Each item has an explicit `source_uri`; playback must use that URI, not its
position or title. Existing `/play` and `/queue` retain all playback policy,
coordinator resolution, DJ ownership and action-log behavior.

The revision is an opaque listing change detector, not a credential or a
playback precondition. It covers the filtered listing's identities and displayed
metadata, not its page offset. A client moving between pages should restart
from the beginning when the revision changes, rather than splice two versions
of a listing together. There is no retained server snapshot. Store reads copy
only the requested category and release the store lock before filtering and
sorting. Response memory is bounded by the page limit; projection still reads
and sorts the selected cache category, as existing search does.

**Cached does not mean currently saved.** The store includes liked tracks,
saved-album tracks, other taste signals, and retired rows. A fresh Spotify read
may change that data. Explicit tracks remain visible; browsing is not DJ
selection. Albums are grouped by their service URI, and `cached_tracks` is only
the number present locally, not the complete Spotify album length. In particular,
an album represented by one liked movement is not advertised as a complete
recording. Play/queue of an album still uses the album URI and the normal Sonos
container path. Playlist membership is recorded separately during sync; open a
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
albums** returns to the album listing. Each album's displayed count explicitly
says it may have more tracks than the cache contains. Choosing a destination
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
never a title search or a mutable page position. Changing category, filter,
page or destination consumes any previous review. Late replies cannot replace
a newer page. A changed cache revision during pagination asks for an explicit
refresh from the beginning; it never combines pages from two listings.

Closing the panel invalidates an unsent action. A failed or unreadable write
is not retried; read the room and queue before deliberately trying again.
The remote uses the same existing embedded library assets and public play/queue
routes, with no new automatic sync, microphone capture or playback polling.
