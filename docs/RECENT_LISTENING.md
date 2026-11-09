# Recent Spotify listening and DJ repeat avoidance

The daemon checks what you recently heard on Spotify independently of its full
saved-library refresh. Listening on a phone or another Spotify device can inform
the next DJ selection without waiting for the daily library refresh.

The read uses the existing optional `user-read-recently-played` grant. It reads
your own account; it does not control Spotify playback, change your library, or
change what is already queued on Sonos.

## When it runs

The existing background worker checks approximately every 15 minutes after the
previous check finishes. A due recent-listening read runs before its normal
library or enrichment pass, including while a large saved-library intake is
unfinished. It does not consume an enrichment turn or rebuild the DJ pool:
the next selection already reads current listening evidence from the store.

One pass makes at most three logical requests: account identity, one window of
recent plays, then account identity again. The existing HTTP client's bounded
retries still apply. The worker shares Spotify cooldowns and authentication
backoff with library reads. Admission and retry deadlines survive restart.
Long-running work or a cooldown can delay a check; this is not a real-time feed.

## Coverage and retention

Each pass reads **at most the latest 50 history rows**. It never follows a
response's continuation URL. Plays that fall outside that window between checks
can be missed; the feature does not import complete listening history.

Repeated occurrences retain each track's latest observed timestamp. Unavailable
items, local files, and non-track entries are counted separately. A malformed
window or a timestamp more than five minutes ahead of the observation clock
rejects the entire observation. A failed read never becomes an empty history.

Accepted plays merge monotonically under the exact account adoption. A delayed
read cannot move a track's timestamp backward, remove another track's evidence,
or publish observations after an intervening account switch, including switching
away and back to the same account. Full library publication uses the same merge
semantics. At most the latest **10,000 distinct track identities** are retained.
Account adoption still clears the previous account's personal listening cache;
local Sonos play history and manual preferences remain separate.

This evidence participates in the DJ's existing repeat-avoidance rules. Those
rules still depend on the available music and current selection constraints.
Previously queued works are not removed or reordered when a new window arrives.

## Inspect freshness

Use the phone/dashboard sync panel, `fsonos dj sync --status`, HTTP
`GET /dj/sync`, or MCP `dj_sync_status`. The optional `recent_listening` object
is independent of `last`, which remains the last completed full-library refresh.

| Field | Meaning |
|---|---|
| `checked_at` | Observation time of the last accepted window, when available |
| `received` | History rows in that window, including duplicates and unavailable rows |
| `accepted` | Distinct valid track identities accepted; not a count of new database rows |
| `skipped` | Unavailable, local-file, or non-track rows |
| `more_available` | An older window was advertised or the 50-row bound was reached; false does not prove complete history |
| `missing_scope` | Sign in again with the recent-listening grant to enable the reader |
| `requires_refresh` | The adopted library account must be refreshed before this reader can publish |
| `retry_at`, `not_before` | Persisted retry and shared cooldown deadlines |
| `error` | The latest failed attempt; a prior matching-account observation retains its original timestamp |

Status contains no account ID, track list, or credentials. Reading status
performs no Spotify request or database read. Workers check retained status
against local account ownership even when a read fails or a full intake remains
unfinished, so another account's old counts cannot claim current freshness.

## Validation

The source includes account-switch, concurrent-publication, malformed-window,
cooldown, actual-next-pick, and browser-status regression tests. The isolated
Rust retention tests and browser regressions ran successfully in this session.
Full Rust workspace compilation and integration-test execution remain blocked
by the existing dependency build's resource requirements in this environment.
