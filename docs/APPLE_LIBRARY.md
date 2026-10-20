# Native collection browsing

Open **Music → Library** in the iPhone, iPad or Mac app. The collection browser
reads the daemon's existing `GET /library/browse` cache. Albums use an adaptive
native grid; playlists and tracks use lists. Filters are debounced. The separate
search mode keeps the fuzzy library and household-favorites search available.

Browsing requires a reachable daemon and cache, not a selected speaker or an
active Spotify connection. It does not start playback, discover speakers or
initiate a sync. Manage sign-in and explicit imports in **House → Spotify &
library**. An unavailable cache is an error, not an empty collection.

## Albums and whole recordings

Opening an album clears the title filter and shows its cached tracks in the
server's disc/track order. Back to albums restores the previous listing and
filter. Counts are labelled **cached tracks**: a liked movement is not proof
that the cache contains the complete recording. Cached rows can also include
retired or taste-derived music, not just current account saves.

**Play album** and **Queue album** use the full album URI, never an invented
batch assembled from its partial cached tracks. An individual track action uses
that exact track URI. Playlist membership is not inferred from the track cache.
Category symbols are placeholders, not claimed album artwork. Accessibility
text sizes use one album column without truncating album names or performers.

## Reviewed playback, not another command implementation

Play now, Queue next and Add to end produce existing native commands and enter
`RemoteModel.prepare(_:in:)`. The command review still owns fresh preflight,
explicit confirmation, policy-authoritative execution and uncertain outcomes.
Queue commands do not send an additional Play or Resume command.

Each row or menu captures its displayed household-qualified destination. If the
selected group changes before a retained menu is used, the action is refused
instead of silently following the new room. The catalog selection also expires
when its accepted page changes. This is client coordination, not an atomic
reservation against another controller.

Each page holds at most 50 items. Pages from different opaque cache revisions
are never combined; a revision change requires an explicit first-page refresh.
Only one read runs, and rapid changes coalesce to the latest navigation intent.
Suspension cancels unsent work and rejects old replies. Failed reads retain an
explicitly stale observation, with its original time and no actionable rows.

## Validation

Run `swift test --package-path apps/apple --filter LibraryCatalog
-Xswiftc -warnings-as-errors` as one command. The catalog tests exercise the
production page decoder, read lifecycle, album navigation and stale selections.
`LibraryCatalogClientTests` exercises the real daemon client using a controlled
transport, exact GET query escaping, whole-album commands, household-qualified
payloads and recovery after an invalid album filter. No live account or speaker
is used. The existing Apple workflow builds both native products and launches
the iPhone connection screen; that launch does not validate a connected library
screen, VoiceOver interactions or physical Sonos playback.
