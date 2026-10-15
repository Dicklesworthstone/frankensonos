# Spotify setup and library refresh from the web remote

The remote's **Spotify setup** and **Spotify library** panels use the existing
daemon sign-in and library-sync APIs. They neither drive Spotify playback nor
change the Sonos household's service linkage. Spotify must still be linked to
each household in its Sonos app.

## Configure the daemon once

The owner must configure `FSONOS_SPOTIFY_CLIENT_ID` (or
`fsonos serve --spotify-client-id`) and the Spotify application's registered
redirect URI on the daemon host. Read the configured callback in Spotify setup;
it must exactly match the application's registration. An unconfigured daemon
reports this prerequisite instead of pretending that a browser can register an
application for it. The existing `fsonos setup` remains available.

## Sign in, then load music

Open Spotify setup and read the current status. **Review new sign-in** explains
that starting replaces any pending consent link, not the cached sign-in. Confirm
only when ready. The link opens Spotify's HTTPS consent page in a separate tab
without an opener or referrer. Only the existing read-only library and taste
scopes are admitted; the daemon holds the PKCE verifier and exchanges the code.

When the registered callback reaches the daemon, return to the remote and check
status. A loopback callback opened on a phone or a different computer may show a
connection error because it points at that device, not the daemon host. In that
case, copy the **full address from the Spotify tab after consent**, paste it in
**Finish from a phone or another computer**, and select **Finish sign-in on
daemon**. Keep the callback private; never send it through chat or logs.

The remote checks that the pasted address belongs to the daemon's configured
callback and, when it retains the consent link, the same flow. It sends only the
code and state as a JSON body to this daemon's completion route. It never visits
or fetches the pasted address. The daemon's state check remains authoritative,
including after a page reload. Duplicate or contradictory callback fields,
unknown destinations, and declined consent do not produce a completion request.

**Cached sign-in available is not a live Spotify access check.** Select
**Continue to library refresh**, then explicitly select **Refresh library from
Spotify**. The first action only opens and reads the refresh panel. The second
asks the daemon to read Spotify into its cache. No step automatically starts,
skips, or restarts music. New DJ picks can use the refreshed cache; already
queued works stay queued.

The refresh panel distinguishes a running attempt from the last completed
counts. It shows failures, unread taste signals or albums, and pending genre
lookups rather than calling a partial refresh complete coverage. Counts and
completion times are the daemon's; times display in the browser's local zone.

## Concurrent clients, failures and private data

Each panel serializes its writes. Sign-in rechecks status before dispatch, but
this client preflight is **not atomic**: another client can replace a pending flow
between observations. Only the daemon decides which flow may complete. The
daemon reserves an exchanging flow until its exchange and cache write finish;
competing starts or callbacks receive `NOT_READY`. A failed session open or
callback preserves that same flow instead of overwriting a newer one.

Lost or unreadable write replies are reported as unconfirmed and recovered with
status reads, never automatic write retries. A reported cached sign-in after an
uncertain reply does not prove which concurrent operation completed. Explicitly
check status and the library read before starting another flow.

There is no browser storage for these values, no third-party script, and no
browser call to Spotify's data API. Closing setup clears its consent link and
callback field; submitting, hiding the page, or disposing the panel clears the
field too. Server error messages are not echoed by the sign-in panel because
they could contain the submitted callback. The existing same-origin JSON policy,
CSP, and daemon client authorization remain in force.

While visible and open, the panels observe pending/running work using one
in-flight status read and bounded error backoff. Closing or hiding pauses those
reads, not the daemon's work; returning performs a fresh read. Stale observations
cannot authorize another write.

## Verification boundaries

`node --test scripts/test-remote-spotify-connect.cjs scripts/test-remote-library-sync.cjs`
exercises parsing, lifecycle and single-dispatch behavior, including actual
localhost JSON requests to fixture servers. Browser component tests use fixture
API replies, not a real Spotify grant or Sonos hardware. Rust unit tests exercise
the sign-in reservation and error recovery; running the Rust workspace and the
owner's real account/hardware checks remains separate from browser tests.
