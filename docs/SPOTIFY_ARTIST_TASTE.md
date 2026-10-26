# Followed artists in the DJ

Following an artist, or listening to them enough for them to appear among your
top artists, can bring their music into the DJ even when you have not saved any
of their tracks. These reads use the owner's existing read-only Spotify grant;
Sonos still performs all playback through its own linked service account.

The artist read first tries Spotify's top-tracks endpoint. Spotify removed that
endpoint for Development Mode apps in its [February 2026 API changes](https://developer.spotify.com/documentation/web-api/references/changes/february-2026).
When it returns 403 or 404, the DJ now reads a small sample from the artist's
albums and singles instead. The rest of that refresh uses the catalog path
without repeatedly asking the unavailable top-tracks endpoint. Apps whose
top-tracks endpoint works continue to use it.

## What enters the pool

Each artist contributes at most three distinct playable tracks. The catalog
sample follows Spotify's returned album and track order; it does not claim to
rank the artist's most popular recordings. Only albums and tracks that actually
credit that artist are used. Local files, unavailable tracks and duplicate
track URIs are skipped.

The cached track retains its album URI, artists, release year, duration, disc
and track numbers, and explicit flag. It is a taste signal rather than a saved
or liked track. The usual preference rules decide whether explicit music may
play, and the artist's taste weight applies as before. Classical selections go
through the same album cache and whole-work completion as other partly held
works. A successful album read completes the recording beyond the three-track
discovery sample; a failed completion retains the existing incomplete-work
behavior and reports why that album could not be read.

## Read bounds and recovery

A full refresh considers at most 50 uncovered followed/top artists. The
existing day-based rotation gives larger lists their turn. For each catalog
sample it reads one directory page of at most ten albums and at most three
unique albums' first track pages, each capped at 50 entries. It does not follow
pagination for this discovery sample. Ordinary whole-work completion remains
a separate step with its existing bounds.

All requests use the normal Spotify session, cancellation checks, deadlines,
retry limits and server cooldown. A failed read keeps the previously cached
music for the unread artists. An empty bounded catalog sample also preserves
that music: examining a few releases cannot prove that an artist has no
playable tracks. A rate limit stops subsequent requests until the session's
cooldown permits another pass.

This change affects DJ taste enrichment. Direct playback of a
`spotify:artist:` URI still requires a separate renderer-resolution path.

## Verification

`cargo test -p fsonos-spotify` includes real loopback HTTP tests for 403/404
fallback, metadata and weighting, deduplication and unavailable entries,
bounded catalog requests, preferred use of working top tracks, retained cache
on failures and rate limits, and complete classical recordings. These tests
exercise the production HTTP client; they do not contact Spotify or physical
speakers.
