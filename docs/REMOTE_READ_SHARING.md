# Shared observations in the full web remote

Independent panels in the full remote at `/` use one JSON request adapter.
Identical ordinary GETs made in the same dispatch burst share one HTTP
exchange, one response-body read and one bounded retry sequence. Each caller
receives a separate copy of the decoded result. This reduces duplicated
observations without introducing a response cache, polling loop or frontend
dependency.

## Freshness boundaries

Sharing closes before the underlying request is sent. A later preflight or
reconciliation read always starts another exchange, even while an earlier
response body is pending. No TTL, delayed batching window or settled reply
cache is used. Paths and queries are compared exactly: household-qualified
room names, queue offsets and revision strings are never normalized together.
Event streams and long polls have their own lifecycle and are excluded.
Requests with additional options or a different representation bypass sharing.

Every non-GET is independent. Starting or completing a command clears sharing
admission, including failed, uncertain and overlapping commands. Reads during
commands do not share. Existing readers can still receive observations that
began before a command; their panels retain their existing generation and
staleness checks. This is not transactional isolation from other controllers.

At most 64 distinct pending sharing keys are retained before dispatch.
Overflow goes through the existing transport directly instead of dropping
reads. This bounds the sharing map, not all network concurrency. The separate
Listening Room dashboard, Apple client, tabs and devices are not coalesced by
this adapter.

## Cancellation without command replay

The `call` adapter accepts an optional AbortSignal. One reader leaving does not
abort sibling readers. The final reader leaving cancels the shared exchange.
The request adapter independently settles cancellation and timeout even when
a test transport or response body ignores abort; it also clears retry waits
and deadline timers.

When the page hides or is suspended, the live-state and catalog recovery lanes
are deactivated before outstanding JSON reads are cancelled. Sent commands are
not cancelled by this read-only cleanup. Explicitly aborting a sent command
still returns an uncertain outcome and never authorizes a retry. The original
read/write deadlines, Retry-After minimums, response validation, same-origin
handling and no-write-replay behavior remain in the shared transport.

Cancellation does not prove that the daemon stopped an already-sent request.
Unresolved network operations that ignore cancellation can remain underneath
the settled caller; later replies do not become successful cancelled reads.

## Verification

```sh
node --test scripts/test-remote-read-requests.cjs scripts/test-remote-request-lifecycle.cjs
```

All 44 tests passed locally. They cover burst sharing, independent result
copies, later preflight freshness, command barriers, individual and page-read
cancellation, ignored-abort bodies, backoff and timeout cleanup, capacity,
household/revision distinctions, and unchanged command payloads. The production
request code and actual page suspension hook are exercised in Node VM fixtures.
Two real loopback HTTP fixtures verify the broker and production adapter: a
50-reader burst shares one exchange, while later reads and commands remain
separate. This measures request count in fixtures, not real-house latency.

JavaScript syntax checks and the Rust asset-registration/privacy checks pass.
The broker is embedded before the production adapter; no optional module is
missing from the served bundle. A standalone adapter without sharing remains
supported for existing embedded tests. The existing Linux workflow includes
both regression files and retains its other gates.

No Rust compiler, Cargo, rustfmt or RCH was available in the editing environment.
Rust compilation, the whole repository test suite, a real browser/daemon event
stream and physical-speaker acceptance are not claimed from these Node checks.
