# Spotify sign-in and library refresh in the native Mac remote

Choose **Spotify & Library** in the connected remote's toolbar. No selected or
reachable speaker is required. The panel uses the existing daemon API, not a
second Spotify client or a local token store. The daemon must already have a
Spotify application client ID and registered redirect URI configured by its
owner. A missing configuration or a policy denial is reported separately from
an unreachable daemon.

## Sign in through the daemon

**Begin Spotify sign-in** requests one consent link. **Open Spotify in browser**
is a separate, explicit action: replies, reconnects and foreground transitions
never open a browser by themselves. The native client checks the exact Spotify
consent origin, authorization-code flow, PKCE S256 challenge and read-only scope
set before handing the link to the system browser. It never accepts a token flow
or a playback/account-write scope from the link.

Authorize the owner's account in Spotify and return to the remote. If the
registered callback reaches the daemon, status refresh observes the completed
sign-in. A loopback callback on another Mac may instead produce a browser
connection error. In that case, paste the **complete address the browser landed
on** into the masked callback field and choose **Complete sign-in once**. The
client checks its origin, port, path, state and code against this consent flow,
then sends it only to the configured daemon's completion endpoint. It never
visits the pasted address. The daemon independently checks state and exchanges
the code using its own PKCE verifier.

The callback field clears on submission, closing, suspension or loss of the
consent link. Neither callback addresses nor consent links are saved in the app's
preferences or shown in error messages. Tokens and the PKCE verifier remain on
the daemon. The user's clipboard and browser history are outside this client's
storage; do not share the return address or a screenshot containing it. Invalid,
denied, duplicate-parameter and mismatched callbacks do not cause a POST.

A pending sign-in may belong to another browser or client. Replacing it requires
an explicit confirmation and invalidates the older flow. Reopening the same
panel may retain a still-valid link in memory; restarting the app does not.
Pending status alone cannot recover a lost link, so use the original browser or
explicitly replace the flow. A cached sign-in reports only that the daemon has a
grant; it does not prove token validity, a finished library import, or working
Spotify service linkage on a Sonos household.

## Refresh the DJ's library

**Refresh library now** sends one request to the existing `/dj/sync` endpoint.
It does not start music, change the owner's Spotify saves, or change a speaker's
service configuration. The daemon refreshes its local cache, and future DJ picks
use that cache. A fresh read first avoids sending another start request when a
job is already running. The daemon remains authoritative for concurrent starts.

Request acknowledgement and import completion are distinct. The panel shows
whether a refresh is running, the last completed pass's timestamp and counts,
retired tracks, pending artist genre lookups and all supplied import notes. A
completed pass with missing data is labelled as such. While a new job runs, older
success counts and earlier failure information remain identified as historical,
not as the current job's outcome. An auth-status read failure does not discard a
valid library observation, and a library failure does not erase sign-in status.

## Lifetime and uncertain outcomes

Setup actions share a single account/cache worker. Opening setup is unavailable
while a playback or schedule review is active; the modal screen does not issue
speaker commands. Sign-in completion consumes its consent link before waiting
for I/O. Double clicks cannot dispatch concurrent setup mutations. Client
preflight reads are not an atomic reservation against another controller.

Only status GETs repeat: normally every two seconds while consent is pending or
a library job runs, and with bounded exponential backoff up to one minute after
read failures. Healthy idle setup does not poll. A suspended/closed panel cancels
its timer and outstanding reads; a rapid reopen waits for cancelled I/O to drain
and rejects obsolete results. The daemon's existing library job may keep running.
No offline request queue or local scheduler is added.

Once a POST is handed to the transport, a lost, invalid or failed acknowledgement
is conservatively uncertain. It is not retried, and arbitrary transport or API
error text is suppressed because it could echo a one-time code. A later successful
status read does not clear or acknowledge that uncertain request. Browser-launch
failure is reported separately and can retry opening the same consent link without
starting a new flow. Disconnecting or exiting discards in-memory setup state;
there is no automatic request replay on the next launch.

## Checks and acceptance

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
Twenty-eight tests exercise the production codecs, daemon client and setup model
with controlled transports. They cover positive sign-in/completion/import,
explicit browser opening, pending-flow replacement, partial imports, read
isolation, unsafe links, exact callback binding, single dispatch, cancellation,
late results, uncertainty and bounded read retries. The full native workflow
also compiles the SwiftUI screen, runs the existing package tests and packages
the release application on macOS.

A separate local smoke check exercised the production URLSession transport over
real localhost HTTP with a synthetic daemon: exact begin/complete/sync request
bodies, a partial-import response, and a 307 response that was not followed or
retried. These checks do not use a real Spotify grant, run a real library import,
configure a Sonos account, validate interactive macOS use or prove tailnet and
physical-speaker behavior. Those remain owner-operated integration checks.
