# Find music already in a queue

Open the group's **Queue**, then use **Find music in this queue**. Enter words
from a title, artist or album and choose **Search queue** (or press Enter).
Typing does not read the queue. Matching ignores case and accents, and every
word must occur somewhere across those three metadata fields. It does not
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

## Bounded sections, not an unbounded scan

One search or continuation makes at most **100 sequential page reads**, each
requesting no more than **100 entries**, and retains at most **200 matches**.
The interface reports the exact position range examined. A small or empty
result set is not called complete unless the end of the queue was reached.

**Search next section** continues after the last examined occurrence. It
replaces the displayed result batch rather than retaining an ever-growing
index. It never runs automatically, and does not skip the rest of a partially
examined page. Starting a new search begins at position one. Large individual
metadata fields are rejected instead of retained without bounds.

**Cancel queue search** stops further reads and discards the unfinished result.
A request already in flight keeps the card's admission slot until its response
settles; cancellation, closing or backgrounding the page cannot create a second
overlapping scan. The search adds no timer, event stream or background polling.
After an error or changed revision, explicitly refresh before searching again.
Search never repeats a playback or queue mutation.

## Verification boundary

Run the model and navigation regressions with:

```sh
node --test scripts/test-remote-queue-search.cjs scripts/test-remote-queue-navigation.cjs
```

The search suite is part of the existing web queue CI step. Its localhost HTTP
fixture verifies browser request ordering and exact-occurrence payloads, not
Sonos hardware behavior. Full daemon, Rust workspace and physical-speaker
acceptance remain separate checks.
