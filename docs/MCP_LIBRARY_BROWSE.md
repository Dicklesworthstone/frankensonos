# Browse music from an agent

`browse_library` makes the existing cached library browser available to MCP
clients over stdio and HTTP. It uses the running daemon when one is selected,
otherwise the backend's shared local Surface. A daemon failure is terminal:
it does not switch to a different cache or return an invented empty list.
The read uses the existing `browse_library` policy permission and bounded MCP
workers. No Spotify request, speaker discovery, library refresh or playback
is triggered by browsing.

Start with `{"kind":"albums","limit":20}` (or tracks, the default, or
playlists). `q` filters title, artist and album words. To inspect an album,
start a new tracks listing with `album_uri` set to its exact source URI and
clear the old title filter. Cached tracks are returned in disc/track order.
Playlist rows are playable sources; this tool does not invent their membership.

The structured response is the same `BrowsePage` as `GET /library/browse`, not
a prose-only list. Pass its `next_offset` and `revision` back as `offset` and
`expected_revision`, keeping category, album and search filters unchanged.
A revision mismatch returns a coded error instead of combining different
cache snapshots. Restart at offset zero without the old revision. Changing
category or filters also starts a new listing. Revisions detect changes; they
are not credentials or playback preconditions and reserve nothing.

Use the selected item's exact `source_uri` with `play` or `add_to_queue` and
an explicitly chosen room. A display position or title is not an identity.
Browsing includes explicit and retired cache entries, and does not establish
that a track is currently saved or available in Spotify. An album's
`cached_tracks` is not its complete catalog length. Refreshing the library is
the separate, explicit `dj_sync` action.

Text output escapes control characters, but structured metadata stays literal.
The client rejects inconsistent page offsets, continuation offsets, duplicate
identities, category mismatches and malformed source URIs. It never plays music
in response to any of those failures.

Targeted Rust regressions: `cargo test --locked -p fsonos-mcp --lib library`.
They cover the real Surface/MemStore projection and injected daemon responses;
physical speakers and live Spotify are not required or simulated as passing.
