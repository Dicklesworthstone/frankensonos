# Spotify setup and library refresh from the web remote

The remote's **Spotify setup** and **Spotify library** panels use the existing
daemon sign-in and library-sync APIs. They neither drive Spotify playback nor
change the Sonos household's service linkage. Spotify must still be linked to
each household in its Sonos app.

## Configure the daemon once

Both the phone remote and desktop dashboard can configure the first Spotify
app on the selected daemon. Open **Spotify setup**, copy the daemon's callback
address into the application's registered redirect URIs, and use **Open Spotify
developer dashboard** to create or inspect the application. Paste its public
32-character Client ID and choose **Save Spotify app**. No client secret is used.

Saving the app enables sign-in on the already running daemon. Its library worker
also becomes available without a restart. Saving does not start consent or
playback; **Review new sign-in** remains a separate action. Client ID drafts
survive errors, panel closing and suspension. After an uncertain save, use
**Check sign-in status** before another action; the remote never retries a write
automatically.

Initial configuration cannot replace an existing app. Repeating the same ID is
safe, and a startup `FSONOS_SPOTIFY_CLIENT_ID` or `--spotify-client-id` keeps its
precedence. The daemon saves its own configuration atomically and preserves
existing comments on same-ID retries. A cached sign-in with a missing original
app configuration requires restoring that configuration on the host.

`fsonos setup` supports the same workflow. When a daemon is selected, setup reads
its callback and saves the app on that daemon, including over the tailnet. A
failed daemon read or save does not fall back to writing local configuration.
With no daemon, the existing local setup remains available. The callback must
exactly match the application's registration in every workflow.

### API contract

`GET /auth/spotify` returns `configured`, `signed_in`, `pending` and
`redirect_uri`, including when no app is configured. `POST
/auth/spotify/configure` accepts only `{"client_id":"<32 hexadecimal characters>"}`
and returns the new status. It uses the same owner-only `spotify_sign_in`
permission as beginning consent; the action log records the operation and
outcome, never the submitted value. Older successful daemon status replies that
omit `configured` are treated as configured by current clients.

## Sign in, then load music

Starting a sign-in is the owner's, since whoever signs in chooses whose
library and taste the DJ follows: the CLI on the daemon's host (`fsonos
setup`) and the tailnet login the daemon's host is logged in as (your own
devices, through Tailscale) may. To start one from another login, grant it
the auth class in the daemon's `policy.toml`:

```toml
[clients."you@example.com"]
grant = ["auth"]
```

Status stays readable without it, and a sign-in the CLI started can still be
finished from the remote.

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
The pending flow retains the app and callback that created it. A later host-side
configuration change cannot exchange its code under another app; restoring the
original configuration makes the retained flow available again.

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
