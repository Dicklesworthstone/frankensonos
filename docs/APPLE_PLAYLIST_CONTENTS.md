# Browse cached playlist tracks in the native remote

In **Music → Browse library → Playlists**, choose **View cached tracks**.
Opening a playlist reads the daemon’s existing `/library/playlist` cache;
it does not contact Spotify, discover speakers, sync the library, or play music.
Browsing works with no selected room. The playlist listing’s filter and page
are retained separately from the track filter. **Back to playlists** reads that
listing again and refuses to combine a changed listing revision.

The detail shows each track’s original one-based playlist position, artist,
album, duration and explicit flag. Repeated recordings remain separate rows.
Unavailable source entries leave gaps; these positions are never Sonos queue
positions. At most 50 occurrences are kept in the visible batch. Previous and
Next carry the same playlist, filter and opaque local revision. A failed page
or changed revision leaves a clearly labelled previous page and requires an
explicit read from the start before acting again.

## Coverage is not a claim about Spotify now

The screen distinguishes a known playlist whose contents have **not been cached**,
a **partial** observation, a **complete empty** observation, and a complete
observation containing **no supported playable tracks**. It shows the original
cache timestamp, source entries examined, source total when known, and cached
playable occurrence count. Filtered or empty pages do not imply that the entire
source playlist is empty. The daemon’s existing bounded synchronization rules
still apply; see [PLAYLIST_CONTENTS.md](PLAYLIST_CONTENTS.md).

Typing edits only the local filter. Press Return or **Read from start / apply
filter** to read it. Cancellation discards late responses and keeps the read slot
occupied until the cancelled work drains. Hiding or backgrounding the remote
retires selections; returning does not automatically reread or make old rows
current again. No background scanner or Spotify synchronization is introduced.

## Reviewed track playback and queue additions

Choose a destination group, then **Play or queue this track**. Play now changes
that group’s source to the selected track; Queue next and Add to end use the
existing queue-add operation without starting playback. These actions choose one
track, not all remaining playlist tracks. The existing whole-playlist actions
in the parent listing remain available separately.

Every selection retains the exact cached occurrence, page revision and
household-qualified group. Preparation and confirmation use the existing
`CommandReviewSession`, rereading both the destination state and the original
cached page. A changed occurrence, revision, group or household blocks the
request before dispatch. Append allows natural track advancement; Play now and
Queue next require the reviewed playback state. The request sends only the
canonical track source to `/play` or `/queue`, never a playlist position to
`/queue/play`. There is no alternate writer or loop of track mutations.

The cache check is not a live Spotify check or an atomic reservation against
another controller. An already sent command can complete after cancellation.
Lost, malformed or redirected acknowledgements remain uncertain and are not
automatically repeated. The daemon remains authoritative for policy and playback.

## Verification boundary

`PlaylistContentsTests.swift` exercises the request/page contract, sparse and
duplicate occurrences, coverage, read-only errors, cancellation, pagination,
and the production shared-review handoff for all three actions in S1 and S2.
`PlaylistBrowserTests.swift` covers listing/detail navigation, stale menus,
return-filter preservation, revision changes and lifecycle cancellation through
the production catalog model and daemon client.

The local Swift 6.2.1/Linux harness passed all 21 new test functions plus 26
retained announcement functions, with warnings treated as errors and three
repeat runs. It compiles the production playlist files, catalog model, complete
NativeCommand, playback-review actor/observation, and transport/read/write paths;
unrelated command/store dependencies are isolated. Removing the new playlist
preflight hook makes the changed-cache regression fail; restoring it passes.

A real localhost HTTP fixture exercised production URLSession and the shared
writer through nine scenarios: play, next, append, continuation, changed cache,
malformed reads, unfollowed redirects, not-cached contents and uncertain writes.
It observed 51 bodyless GETs and five exact POSTs without automatic retries,
Spotify sync or queue-position playback. Responses are synthetic, not a running
Rust daemon. The complete changed view parses for iOS 17 and macOS 14; parsing
is not Apple SDK type-checking or rendered interaction. Full native-package,
Apple SDK, live-daemon and physical Sonos acceptance remain separate checks.
