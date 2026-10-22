# Search the native listening queue

In the iPhone, iPad or Mac remote, select a group and open **Up next → Find
music in this queue**. Enter words from a title, artist or album and press
**Search queue**. This searches the existing queue, not the Spotify catalog.
Opening the panel and typing do not send a search or change playback.

The native remote uses the same read-only daemon search as the CLI and MCP:
`GET /zones/{room}/queue/search`. It retains each matching occurrence's original
position and opaque revision. Two copies of a recording remain two results.
The client checks current household/group membership before each scan and
retains the existing transport's response limits and no-redirect behavior.

Each batch retains at most 50 matches. The coverage line identifies exactly
which positions the daemon examined. **Search next section** carries the
revision and first unexamined offset; it replaces the visible batch. It does
not run automatically. An empty section is not presented as an empty queue,
and reaching the end does not imply earlier batches are still displayed.
The daemon's scan budgets and continuation semantics are documented in
[QUEUE_SEARCH.md](QUEUE_SEARCH.md).

**Review playing this occurrence** hands its original position, renderer URI
and revision to the existing one-use command review. Confirmation rereads the
queue before sending the existing guarded queue-play command. It never
converts a renderer URI into a new source or adds a duplicate track. Results
without a usable URI remain informational. Search matches are not treated as
a contiguous selection for block movement or deletion.

Changing the query or selected group, closing the panel, backgrounding the
app, a connection change, a catalog invalidation, or an observed queue revision
change retires the results. Retained results say **Previous results** and cannot
start a review. No event or refresh automatically reruns a search. Ordinary
clock progress and an unchanged queue revision do not retire an observation.

Cancellation stops unsent work and discards late replies. The read slot remains
occupied until the cancelled operation drains, including transports that ignore
cancellation. A failed continuation retains only a stale previous batch and
requires a new search from the start. No search acknowledges or retries an
uncertain playback command. Client checks are not an atomic reservation against
another controller; the daemon remains authoritative.

## Verification boundary

`QueueSearchTests.swift` covers the contract, production read adapter and
single-flight lifecycle, including S1/S2 requests, sparse duplicate occurrences,
coverage consistency, cancellation, stale selections and continuation failures.
`QueueSearchIntegrationTests.swift` exercises the handoff through the real
`CommandReviewSession`, changed revisions and uncertain playback receipts.

The local focused Swift 6.2.1/Linux harness ran the first suite with
warnings-as-errors. It includes the production transport/read adapter and
new search model, but isolates unrelated models and the NativeCommand enum
handoff; it does not execute the full shared writer. A localhost HTTP fixture
also tested the production URLSession path, exact continuation queries, API
refusal, malformed replies, an unfollowed redirect and regrouping with no
playback calls or retries. These are synthetic responses, not a Rust daemon.
Full-package integration tests, Apple SDK type-checking, rendered UI and
physical-speaker acceptance remain separate gates.
