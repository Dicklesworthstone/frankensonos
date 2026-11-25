# Spotify read recovery

Library synchronization, taste signals, artist genres and album completion use
`Session::get` or `Session::get_brief`. Both now recover read-only requests from
transient transport failures and HTTP 408, 429, 500, 502, 503 and 504 responses.
The session makes at most four GET attempts, counting an attempt retried after
an access-token refresh in the same budget. Failures cannot reset the counter.
The injected HTTP client's own retry policy is unchanged.

The retry delays start at 250 milliseconds and double. `Retry-After` accepts
integer seconds or an HTTP date and is a minimum, not a value to clamp into an
early retry. Missing or malformed 429 delays default to one second. An integer
too large to represent refuses retry rather than becoming a short default.
A background read refuses waits longer than five seconds; a grant check does
not retry transient failures or wait for a rate limit, but still refreshes once
when the access token is rejected.

One total deadline covers GET connection and response-body reads, token refresh
and backoff: 90 seconds for an ordinary read, 35 for a background read and 30
for a grant check. The caller's earlier context deadline wins. Individual HTTP
requests retain their 30-second ceiling, shortened by the remaining read
budget. A delay that cannot fit returns the original API failure immediately.
Cancellation is checked before dispatch and during waits; backoff sleeps check
at intervals no longer than 100 milliseconds, subject to runtime scheduling.
Cancellation, exhausted deadlines, TLS/configuration failures and malformed or
oversized HTTP messages are not classified as transient.

Only Web API GETs are retried. Authorization-code and refresh-token POSTs retain
their existing single-submission session path. The existing endpoint guard runs
before authentication or network dispatch. No new OAuth scopes, playback
commands or dependencies are introduced. Successful
response bodies still pass to their existing parsers; malformed JSON is not a
reason to replay a request.

## Account changes while reading

A Spotify read stays attached to the sign-in that started it. If that cached
sign-in is replaced or removed, an old session stops before sending another
request. If the change happens during a request, its response is discarded with
an authentication error before it can become a search result or playback
selection. Retry the catalog request with the current sign-in; each catalog
request opens a new session.

Automatic token refresh still works: a session can continue reading with the
credentials it refreshed itself. The private token cache now keeps an opaque
sign-in receipt beside the credentials. Refreshing or rotating a token preserves
that receipt; completing a new authorization changes it, even if Spotify
returns the same token strings. Existing token files remain readable, and their
receipt survives the first refresh without requiring another sign-in. An
observed Spotify rate limit is retained even when the account changes before
its response is accepted.

The running daemon notices a replaced grant at its existing half-second check,
even when it was already signed in. If another refresh owns the worker or its
directory lock, that wake stays pending until admission succeeds. A bounded
full library read takes priority
over account-dependent listening and enrichment jobs. This admission survives
restart, retires the previous grant's completed proofs and retry backoff, and
preserves the application's Spotify cooldown. Ordinary token refresh does not
request an extra full library scan. A reader already in flight cannot
acknowledge a replacement grant or publish its old progress as the new sign-in's
completed refresh.

Until that sign-in has a completed full refresh, new DJ starts, rehearsals,
readiness checks and refills cannot use the retired selection pool. The DJ
rechecks the receipt before appending selected tracks, including after speaker
preflight; already queued music and explicit stop keep their normal behavior.
Removing credentials after admission also keeps new selections waiting.
Inspect `fsonos dj sync --status` for the pending refresh, or request
`fsonos dj sync` to run the next bounded pass. On upgrade, an existing authorized
library needs one full refresh to establish this receipt; its last successful
refresh remains visible while the new read is pending.

## Availability survives restarts

A completed library refresh retains Spotify's explicit track exclusions for
the accepted account. This includes unavailable liked tracks and fresh taste
observations, even when those tracks contribute no playable library row.
Migration 25 stores the exclusions beside the local library. Publication changes
them in the same transaction as the library and its saved-read, playlist and
artist receipts. An incomplete scan, malformed response, or stale publication
cannot replace the accepted evidence.

This matters when the DJ completes a liked classical movement from an older
album cache: the cache cannot put an excluded movement back into the work.
Both offline and network-assisted completion apply the retained exclusions,
preserve the original recording boundaries and mark a known missing movement
as partial. Ordinary songs use the same candidate exclusion. Already queued
music is left in place; the catalog revision invalidates cached plans before
new selections.

A fresh playable observation accepted by a full refresh can restore the track.
Its absence from a response cannot. Reused playlist pages, cached taste fallback,
album-cache refreshes and additive background readers retain prior exclusions
instead of treating older positive metadata as new permission. A change to the
accepted Spotify account clears the previous account's exclusions.

Background artist sampling also publishes fresh explicit denials, including a
pass that returns no playable tracks. The denials, accepted music and exact
cohort receipt commit together under the current account. A negative-only change
advances the catalog revision, so a warmed DJ pool stops selecting that track
without waiting for a full library refresh. A newer full refresh or account
change rejects an older in-flight artist result. Background positive metadata
cannot clear an exclusion; that still requires a fresh playable observation
accepted by a full refresh.

Saved-library checkpoints include unavailable identities in their page proofs.
Version 2 checkpoints verify those identities when resuming; an older private
checkpoint restarts intake without replacing the public library. Evidence is
bounded, and overflow refuses publication rather than forgetting exclusions.

## Successive reads on the same session

A 429 records a monotonic cooldown before returning or waiting, including the
last permitted attempt. Later Web API reads check that cooldown before obtaining
an access token or sending a request. This prevents taste/genre/album stages
from immediately making fresh requests after a preceding stage returned early
for a long rate limit. Grant checks return 429 without waiting; other reads wait
only when the full remaining cooldown fits their wait and total-read budgets.
Dropping a waiting future does not clear the cooldown.

The cooldown uses elapsed process time, not a reinterpreted wall clock or a new
runtime context's timer epoch. Changing the configured API endpoint clears its
old peer's limit; reinstalling the same endpoint and refreshing credentials do
not. Huge unrepresentable delays fail closed for that session rather than
wrapping into an early retry. This state is local to a Session: separate sessions
and processes are not coordinated, and a restart does not persist the limit.

## Verification

`session/retry/tests.rs` adds pure policy checks and a scripted loopback TCP
peer exercised through the real asupersync HTTP client. Cases cover recovered
reads, bounded mixed failures, token refresh, lost responses, rejected delays,
terminal errors and deadlines spanning body reads, backoff and refresh. Six
additional cases cover cross-endpoint cooldown, expiration, early-return grant
checks, dropped waits, last-attempt rate limits and endpoint changes. The
runtime test futures own their session state, matching the existing test style.

Credential regressions change the cached sign-in after the peer receives a
request and before it replies, without timing sleeps. They cover fresh-token
replacement and removal, successful and failed responses, connection loss,
unchanged-credential rewrites, and successful proactive and reactive refresh.

Reauthorization cases also cover opaque receipt persistence, two real PKCE code
exchanges, rotating HTTP refresh, stale full-reader completion, removed grants,
and warm/cold DJ pool admission. The real daemon watcher runs with a fixed clock
to prove that replacement wakes it before the periodic deadline, including
while another writer holds the refresh lock. Catalog cases replace consent
during pool construction and speaker preflight, before the first queue append.
These new receipt, watcher and catalog tests await Rust execution: the
implementation environment filled during dependency compilation and became
unavailable before the project test binaries could run.

The focused filters below exercise this behavior; live Spotify operation and
physical-speaker acceptance are separate. The stalled-body
case uses an interruptible 30-second scripted body against a one-second total
budget, so loaded-host scheduling cannot turn it into a zero-request race. Run:

```sh
cargo test -p fsonos-spotify client::tests::
cargo test -p fsonos-spotify session::
cargo test -p fsonos-cli dj::sync::
cargo test -p fsonos-cli dj::catalog::
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
