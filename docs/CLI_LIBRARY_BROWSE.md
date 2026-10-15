# Browse cached music from the terminal

`fsonos dj browse` reads the music cache used by search and the DJ. It works
without starting the DJ, discovering speakers, or requesting Spotify. Refresh
is a separate explicit `fsonos dj sync` operation.

```sh
fsonos dj browse albums --limit 20
fsonos dj browse tracks --album-uri spotify:album:ALBUM_ID
fsonos dj browse playlists --q 'dinner'
fsonos --json dj browse tracks --q 'Björk'
```

An album drill-down returns cached tracks in disc/track order. The album's
cached track count is not a claim that the complete album has been synced.
Playlists are named playable sources, not listings of their membership.
Explicit and taste-derived entries remain visible. Entries may no longer be
saved or playable on Spotify; the response always identifies coverage as cached.

## Continue the same listing

JSON output is the same `BrowsePage` as HTTP and MCP: `items`, `total`,
`next_offset`, `revision`, category and filters. Use the previous `next_offset`
with `--offset`, and the previous `revision` with `--expected-revision`, keeping
the same category and filters. Limit is 1–50. Each invocation reads one page.

```sh
fsonos --json dj browse albums --limit 20 --offset 20 --expected-revision CACHE_REVISION
```

A changed revision fails rather than mixing old and new pages. Start again at
offset zero without the previous revision. Changing category or filters also
starts a new listing. The revision is a change detector, not a reservation.
Select an exact `source_uri` for a subsequent play or queue command; display
positions and titles are not stable identities.

## Choose the cache authority

By default the command uses the selected running daemon. `--daemon` requires
it; `--daemon-url` uses the existing remote/tailnet client. Once it answers,
a failed or malformed browse reply does not fall through to another cache.
`--direct` skips the daemon and reads the configured local store. A store that
cannot open is an error, not a successful empty listing. Neither mode scans
the speaker LAN or synchronizes Spotify as a side effect of browsing.

The CLI and MCP use the same API query encoder, page validator and text
renderer. Human-readable metadata escapes terminal control characters;
structured metadata remains literal. Cached coverage, explicit content and
continuation guidance appear in text output too.

Targeted tests: `cargo test --locked -p fsonos-cli --test library_browse_cli`
and `cargo test --locked -p fsonos-cli --bin fsonos dj_cmd::browse`.
The process tests use a real binary and loopback HTTP with synthetic replies;
the direct-cache case uses a temporary local store. They require no physical
speakers or live Spotify account and do not establish hardware acceptance.
