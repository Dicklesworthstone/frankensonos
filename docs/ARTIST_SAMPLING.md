# Background artist sampling

Following an artist can supply music for the DJ even when none of that artist's
tracks are saved. A successful full library refresh samples up to 50 followed
or top artists, then retains a finite cohort of unfinished artists. The daemon
continues that cohort in bounded background passes instead of requiring repeated
manual or daily full refreshes.

This is small catalog sampling. Each attempted artist contributes at most three
distinct playable tracks. The existing artist top-tracks endpoint is preferred;
a 403/404 uses the bounded album-catalog fallback. Sampling does not crawl an
entire discography, control Spotify playback, or request new write permissions.

## What continues

The cohort uses stable artist IDs, so artists with the same display name remain
distinct and renamed profiles keep their place. Followed and top signals are
deduplicated, with their taste weights captured by the accepted full refresh.
Those retained signals also preserve the existing treatment of guest credits.

Each background pass attempts at most 50 artists. Successful artists leave the
pending set; unavailable or temporarily failed artists yield to later artists,
retain their accepted cached music, and retry with bounded backoff. A global
halt or Spotify cooldown does not consume artists whose reads were never
attempted. Once the pending cohort is empty, automatic sample reads stop until
another full refresh establishes new work.

For example, with 120 eligible artists and successful samples, the full refresh
can sample 50, then two background passes can sample 50 and 20. A restart between
those passes retains the remaining work. The background passes do not reread the
saved library or artist directories and do not change the last full-refresh
timestamp.

## Publication and concurrent readers

New samples are added to the current candidate library in one transaction.
Existing active saved-track metadata and stronger taste weights are preserved;
unrelated candidates, playlists and listening evidence are retained. Artist
progress advances in the same transaction, so a failed publication cannot skip
a batch.

Every publication checks the exact Spotify account adoption and generation plus
the previous artist-progress receipt. An account switch, including switching
away and back to the same account ID, invalidates an older pass. A competing full
refresh or background publication also invalidates an older receipt. No copied
whole-library snapshot is used to publish the background samples.

Receipt-only scheduling progress does not invalidate the DJ's candidate pool.
Actual candidate or weight changes advance the catalog revision, so new music
becomes available to future selections without changing already queued music.
The existing genre and whole-work album readers can enrich newly discovered
music afterward.

## Scheduling and status

Artist sampling takes a fifth fair continuation turn alongside playlist, album,
genre and followed-directory work. Admission and retry deadlines survive a
daemon restart, and all these readers obey the shared Spotify cooldown. A
failed sample pass does not make the previous full refresh appear newer.

Use `fsonos dj sync --status`, the existing HTTP/MCP sync status, or the web sync
panel. The optional `artist_sampling` status reports:

| Field | Meaning |
|---|---|
| `pending` | Artists still unfinished in this cohort. |
| `completed` | Successful samples in the current cohort, including its full-refresh start. |
| `attempted` | Artists attempted in the most recent bounded pass. |
| `running` | An artist-sample pass is active. |
| `retry_at` | Earliest next attempt after the pass's backoff. |
| `not_before` | Shared Spotify cooldown, when active. |
| `limited` | Private evidence reached its bound and automatic sample reads are paused. |
| `requires_refresh` | A full refresh must establish a valid account and cohort. |

The status omits account identities, artist IDs, private receipts and the
cohort's internal cursor. Older status readers can ignore the optional field.
Older saved cursors remain readable; the next full refresh seeds their finite
background cohort.

Private progress is limited to 101,000 artist signal IDs (the 100,000-artist
followed-directory bound plus up to 1,000 top artists), 16 MiB of encoded
metadata, and seven days. An expired cohort waits for a new full refresh.
Invalid or oversized evidence cannot authorize unbounded reads. Reaching a
private limit leaves the accepted full-library result usable and reports the
paused sampling state.

## Regression coverage

Regression cases cover memory and SQLite publication, database reopen,
competing full/background reads, returning accounts, metadata and weight
preservation, atomic rollback, bounded loopback catalog sampling, cooldowns,
restart scheduling and the shared web status panel. Check the matching CI run
for executed Rust validation; source review and formatter checks do not replace
running the cases.
