# Browse music in the Listening Room

Open **Find music** in `/dashboard`, then expand **Browse cached library**.
Choose Albums, Tracks or Playlists, filter within that category, and use the
previous/next controls to browse 50 entries at a time. Album rows open their
cached tracks in the server's disc/track order. Opening an album clears an
album-title filter so it cannot silently hide the movements inside.

This is the daemon's existing `/library/browse` cache, not a new Spotify client.
Browsing does not need a selected speaker, does not request a new account grant,
and does not initiate a sync. Old and incomplete cached entries remain possible.
Album counts say *cached tracks*: playing or queueing the full album uses its
album URI, not a reconstructed list of only the tracks currently cached.
Playlists likewise use their source URI and may have changed at the service.

## Play or queue what you selected

Select a fresh listening group in the dashboard, then choose **Play now**,
**Add to end**, or **Queue next** on a catalog row. These open the existing
command review, naming the exact group, source and effect. Play now replaces
the current source, and album/playlist playback replaces the group's queue.
The two queue actions add music without starting paused playback.

For several individual tracks, use the row checkboxes, then **Queue selected
next** or **Add selected to end**. The review lists up to ten exact occurrences
in their catalog order, including repeated URIs, before one `POST /queue/tracks`.
The action preserves playback and has one undo record. Selection stays within
the current page and clears on navigation or filtering. See
[Ordered track selections](QUEUE_TRACK_SELECTIONS.md) for the destination queue
check, partial-failure behavior and equivalent CLI/MCP calls.

Confirmation uses the same single-command lane as transport, DJ and scene
controls. It checks playback/group identity, rereads the exact catalog page,
and verifies its revision and selected entry before one native `/play` or
`/queue` request. Changed sources, titles, explicit flags, album cache counts,
room membership or household identity require a new selection/review. This is
an optimistic preflight, not an atomic reservation against another controller
or service-side playlist edits. Daemon policy remains authoritative.

Closing the chooser invalidates its selections. Hidden pages, disconnected
observations and cancelled reviews cannot dispatch a dependent command after
a late preflight reply. A command already sent is not cancelled or repeated.
An unreadable or lost acknowledgement retains the dashboard's uncertain state
until an explicit successful playback Refresh. Queue expansion has a finite
ten-minute response budget; this is not a guarantee about service completion.

## Navigation and recovery

Reads are serialized with only the latest queued navigation intent retained.
Typing immediately removes the old rows' selection authority; filtering waits
300 ms after typing rests, or Enter submits it. Late responses cannot replace
a newer query or revive a closed/suspended view. Healthy listings do not poll.

Pagination never combines cache revisions. A revision change leaves the old
page visibly stale with selection/paging disabled; **Refresh listing** restarts
at page one. A failed read is not an empty library. Correcting an invalid filter
allows normal refresh again. Same-title items keep distinct source identities.

## Checks

```sh
node --test scripts/test-dashboard-library.cjs scripts/test-dashboard-library-actions.cjs scripts/test-dashboard-session.cjs scripts/test-library-track-selection.cjs
python3 scripts/test-dashboard-library-browser.py --chromium /path/to/chromium
```

The Node suites cover catalog and command state, actual selection controls,
ordered review, repeated occurrences, destination changes, cancellation and
uncertain HTTP replies. Chromium component cases exercise the shipped catalog,
core, session and workspace modules, including review handoff, retained targets,
hidden preflight, failed acknowledgement, and long metadata at 320/390/768/1440
pixels. Browser hosts, HTTP responses and unrelated queue collaborators are
fixtures; these checks do not constitute full-page or live-daemon acceptance.

The Node suites are registered in Linux CI. The ordered-selection API and its
CLI/MCP adapters also have Rust request and simulator regressions; see
[Queue track selections](QUEUE_TRACK_SELECTIONS.md). Full workspace execution,
full-dashboard layout and physical Sonos/Spotify/Tailscale acceptance remain
separate checks.
