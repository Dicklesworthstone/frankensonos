# Find music already in a queue

Open the group's **Queue**, then use **Find music in this queue**. Enter words
from a title, artist or album and choose **Search queue** (or press Enter).
Typing does not read the queue. Matching folds case, common Latin accents and
punctuation, and every word must occur somewhere across those three metadata fields. It does not
search the Spotify catalog or add anything to the queue.

Each result keeps its original position. Two copies of the same track remain
two results rather than being collapsed by title or URI. Unknown metadata is
not guessed, and an entry without a usable renderer identity cannot open an
actionable result.

## Inspect an occurrence, then decide what to do

**Show position** reads the page containing that exact occurrence again. Both
the opaque queue revision and the renderer URI must still agree with the
search result. The queue list marks and focuses the found entry. Opening it
does not play, seek, select, move or enqueue anything. Use the ordinary queue
controls for a subsequent deliberate playback or reviewed edit.

The selected group remains household-qualified for every request. Existing
cross-page selections are preserved only while their identities and revision
agree; search and navigation consume any earlier move/removal confirmation.
A changing queue invalidates a search rather than joining pages from different
queues. An old search result cannot navigate after editing the query, refreshing,
closing the queue, or receiving stale live-state evidence.

## Web remote search limits

The phone remote and Listening Room use the daemon's shared native search.
Each explicit search or continuation submits one bounded search request, so a
phone on your tailnet does not fetch the queue page by page. The phone retains
at most **200 matches** and the dashboard at most **50 matches** per section.
Both report the exact position range examined. A small or empty result set is
not called complete unless the daemon reached the end of the queue.

The request carries the opened queue's revision, including for the first
section. The client checks the returned query, revision, queue length, ordered
occurrences and continuation coverage before displaying results. A result that
contradicts an already observed occurrence requires a fresh queue read. Query
text is bounded to 1024 UTF-8 bytes, and metadata fields to 16 KiB each.
Matching is performed by the same implementation used by CLI and MCP search.

**Search next section** continues after the last examined occurrence. It
replaces the displayed result batch rather than retaining an ever-growing
index. It never runs automatically, and does not skip the rest of a partially
examined page. Starting a new search begins at position one. Large individual
metadata fields are rejected instead of retained without bounds. A section
that finds no matches can still offer continuation after the last examined
position; continuations never advance by the number of matches.

**Cancel queue search** discards the unfinished result and prevents continuation.
A request already in flight keeps the card's admission slot until its response
settles; cancellation, closing or backgrounding the page cannot create a second
overlapping scan. The search adds no timer, event stream or background polling.
After an error or changed revision, explicitly refresh before searching again.
Search never repeats a playback or queue mutation. Search reads have a
60-second whole-request deadline, allowing the daemon's 15-second scan budget
and final speaker checks to finish. Ordinary queue reads keep their shorter
deadline, and read retries share the original search deadline.

## Search from the terminal or an agent

The CLI and MCP use the daemon's shared queue search, or the same implementation
directly on the LAN when the CLI runs in direct mode:

```sh
fsonos queue-search "Living Room" "dvorak prelude"
fsonos queue-search "Living Room" "dvorak prelude" --limit 20 --json
```

The HTTP equivalent is
`GET /zones/{room}/queue/search?q=<words>`; the MCP tool is
`search_queue`. Both accept `offset`, `limit` and
`expected_revision`. The CLI spells these `--offset`,
`--limit` and `--expected-revision`.
All use the existing `get_queue` policy permission.

Results contain the absolute queue position, title, artist, album and exact
renderer URI of each matching occurrence, plus the queue revision. Matching
folds common Latin accents, case and punctuation, and every query word must
occur somewhere in the title, artist or album. Repeated copies of a track
remain separate results. An occurrence whose speaker reports no usable URI
remains informational; its identity is never invented.

A response describes one section of the queue:

| Field | Meaning |
|---|---|
| `total` | Entries in the queue, including nonmatches |
| `offset` | Entries before this section; zero for a new search |
| `examined` | Entries examined in this section, including nonmatches |
| `next_offset` | First unexamined offset, when more of the queue remains |
| `complete` | This section reached the end of the queue |
| `revision` | Opaque observation to carry into continuation or guarded playback |

The default result limit is 50, with a maximum of 200. A section reads at most
99 data pages of up to 100 entries each, plus a final page recheck. It also
yields a continuation after a 15-second scan budget, checked between pages;
an in-flight speaker request and the final consistency checks can take longer.
It returns fewer entries when it reaches the result limit or end first.
A small result set is therefore not evidence that the search reached the end;
inspect `complete` and `next_offset`.

To continue, send the returned `next_offset` as `offset`, the returned
`revision` as `expected_revision`, and the same room and query. A nonzero
offset requires the revision. If the group or queue changed, the search
returns `QUEUE_CHANGED` instead of mixing observations; start again at
offset zero without the old revision.

Search itself never plays or edits music. To play a result, use its
`position`, `uri` and `revision` as the existing
`play_queue` operation's `position`, `expected_uri` and
`expected_revision`. Search matches can be far apart: never treat the
result array as a consecutive range for removal or movement.

## Verification boundary

Run the model and navigation regressions with:

```sh
node --test scripts/test-remote-queue-search.cjs scripts/test-remote-queue-navigation.cjs scripts/test-remote-requests.cjs
```

The search suite is part of the existing web queue CI step. Its localhost HTTP
fixture verifies browser request ordering and exact-occurrence payloads, not
Sonos hardware behavior. The shared search, HTTP route, CLI and MCP have additional Rust regressions:

```sh
cargo test -p fsonos-api --lib surface::queue::search
cargo test -p fsonos-api --test queue_search_http
cargo test -p fsonos-cli --bin fsonos queue_search_cmd
cargo test -p fsonos-mcp --lib queue_search
cargo test -p fsonos-cli --test e2e_queue_search --test mcp_stdio
cargo test -p fsonos-mcp --test worker_tools
```

These use parser fixtures and virtual S1/S2 speakers on localhost. Physical
speaker acceptance and the complete workspace gates remain separate checks.
