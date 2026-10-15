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
commands, dependencies or token persistence rules are introduced. Successful
response bodies still pass to their existing parsers; malformed JSON is not a
reason to replay a request.

## Verification

`session/retry/tests.rs` adds pure policy checks and a scripted loopback TCP
peer exercised through the real asupersync HTTP client. Cases cover recovered
reads, bounded mixed failures, token refresh, lost responses, rejected delays,
terminal errors and deadlines spanning body reads, backoff and refresh.

These Rust regressions were authored but **not executed in the editing
environment**, which has no Rust compiler, Cargo, rustfmt or RCH. Baseline and
candidate Git blob hashes and whitespace/patch checks were verified. This is
not a claim of compilation, integration or live Spotify acceptance. Run:

```sh
cargo test -p fsonos-spotify session::
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
