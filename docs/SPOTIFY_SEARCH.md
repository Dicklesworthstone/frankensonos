# Spotify catalog search

Find music or podcasts beyond the saved library, choose a result, then play it
through the same guarded Sonos controls used for pasted Spotify links.

## Find and choose

The phone remote and dashboard source selector offer **Spotify catalog**.
Enter a query, choose one content kind, and press **Search**. Typing does not
send requests. Select a result to put its canonical source into the link draft,
then review the target group and **Play now**, **Play next**, or **Add to queue**.
Selecting a result alone never writes to a speaker.

Use **Next results** and **Previous results** in either browser selector to read
another page. Paging keeps the same query and category and retires prior result
buttons while the read is pending. Empty pages can still offer a next page when
Spotify returned unavailable entries. Backward navigation rereads the provider;
it does not restore stale selection authority. Editing the query or category,
changing the destination, cancelling or closing retires paging and late replies.
There is no automatic catalog crawl or background paging.

### Build an ordered selection in either browser

Select up to ten tracks or podcast episodes across search pages, queries and
categories. The selected list shows each title, exact Spotify URI and original
search. Move entries up or down, remove them or repeat an entry; repetitions
count toward the ten-entry limit. Searching for more music keeps the list.
Albums, artists, playlists and shows retain their individual **Use this source**
action.

Choose a listening group or a room set, then review **Play selected now**,
**Play selected next**, or **Add selected to queue**. The review shows the
complete sequence and every affected group's current source and queue.
Changing the destination keeps the chosen music and retires the old review.
Closing the source view, clearing its draft or leaving Spotify catalog clears
the selection.

Before review and again before confirmation, the remote rereads each distinct
original search page and checks the selected URI, title and subtitle. Changed
ranking within that page is allowed; a disappeared or changed result requires
updating the selection. These reads also retain the normal Spotify sign-in,
cooldown and error behavior. The dashboard's review lifetime starts after
inspection completes, allowing bounded provider reads to finish while group,
selection and destination checks remain active.

Confirmation sends one ordered source-control request through the existing
command lane. It preserves deliberate repeats, each group's result and queue
insertion receipt, including partial or uncertain outcomes. A lost reply never
causes an automatic playback or queue retry.

### Native iPhone and Mac app

Open **Spotify online** in the **Music** library panel. Choose one of the same six categories,
enter a query and press **Search Spotify**. The native client uses the daemon's
existing account; it can search without a selected speaker and does not create a
separate Spotify sign-in. Ordinary local reads retain their usual deadline,
while this online search allows 60 seconds for the daemon's 45-second worker.

Results retain the exact query, kind, canonical source URI and observation time.
Each page contains at most ten results. Use **Next results** and **Previous
results** to read another page while keeping the same query, category and limit.
Navigation follows the daemon's raw positions, so a page containing only
unavailable entries can still lead to more results. Previous results rereads the
provider at the earlier observed position. A missing continuation asks the user
to refine the query when more results exist beyond the provider's bound.

Search and paging are explicit and single-flight. Changing the query or
category, cancelling, leaving the view or suspending the app retires paging and
selection authority. A cancelled request keeps its slot until it drains, and
late replies cannot overwrite a newer draft. Starting a new search begins again
at the first page. Older daemons remain usable for their first page without
guessing a continuation from their displayed item count.

**Play now**, **Queue next** and **Add to end of queue** enter the same one-use
review as a pasted source. Review binds the chosen source to the selected group;
confirmation never reruns the search. Queue insertions and artist/show queue
replacement also compare a bounded destination queue observation before sending.
A changed group or queue requires a fresh review. Lost playback replies remain
uncertain and are never automatically retried.

### CLI

The CLI defaults to tracks and ten results:

~~~bash
fsonos spotify-search "Nina Simone" --kind artist
fsonos spotify-search "science" --kind show --limit 5
fsonos spotify-search "science" --kind show --limit 5 --offset 5
fsonos --json spotify-search "title words" --kind track
~~~

Use a returned source URI in a separate request:

~~~bash
fsonos play Kitchen 'spotify:artist:<returned-id>'
fsonos play Kitchen 'spotify:track:<returned-id>' --next
fsonos play Kitchen 'spotify:episode:<returned-id>' --queue
~~~

`--direct`, `--daemon`, and `--daemon-url` work as usual. A selected daemon owns
the Spotify session and result. A failed remote search does not silently switch
to another account or the local saved library.

## HTTP and MCP

| Surface | Request |
|---|---|
| HTTP | `GET /spotify/search?q=Nina%20Simone&kind=artist&limit=10` |
| MCP, Full profile | `search_spotify` with `{"query":"Nina Simone","kind":"artist","limit":10}` |
| CLI | `fsonos spotify-search "Nina Simone" --kind artist --limit 10` |

The HTTP endpoint requires `q` and `kind`; the MCP tool requires `query` and
`kind`. The optional limit defaults to ten; `offset` defaults to zero and accepts
0–1000. The six kinds are `track`, `album`,
`artist`, `playlist`, `show`, and `episode`.

The response contains the normalized `query`, `kind`, ordered `items`,
`more`, `offset` and nullable `next_offset`. Each item contains `source_uri`,
`title`, and an optional `subtitle`. Sources are canonical Spotify URIs with
the requested kind. An empty `items` array can still carry a continuation:
unavailable rows and duplicate sources count toward the provider position.

To read another page, keep the exact query, kind and limit, and pass the returned
`next_offset` as HTTP/MCP `offset` or CLI `--offset`. Never calculate it from
the visible item count. Each request reads only one page. A null `next_offset`
ends available continuation; `more` can remain true at Spotify's offset limit
or for an older custom provider that cannot expose raw paging. Refine the query
in that case. Search order may change between requests; this is not a frozen
catalog snapshot. Older first-page daemon replies remain readable, but cannot
satisfy a nonzero-offset request.

Search reads metadata only. An agent chooses a result and then calls the
existing `play` or `add_to_queue` tool. House policy, group/queue checks,
renderer discovery, and bounded artist/show resolution remain part of that
separate playback request.

## Account, bounds, and errors

Search uses the owner's existing Spotify app configuration and sign-in from
`fsonos setup`. No new OAuth scopes or Spotify playback permissions are added.
The current account supplies its country. Search shares the catalog worker's
admission slot, overall 45-second deadline, token refresh and retained Spotify
cooldown with artist/show resolution.

Queries must contain 1–256 UTF-8 bytes after trimming outer whitespace and must
not contain control characters. Limits are 1–10 and offsets are 0–1000. Each
request rebuilds its explicit provider URL against the signed-in session's
configured endpoint; returned next-page URLs and artwork are never fetched.
Available entries retain provider order; duplicates, null rows and entries
marked local, restricted or unavailable are omitted. Malformed usable
identities, metadata or page counters fail the whole response instead of
offering an untrusted source.

Titles are bounded to 4096 UTF-8 bytes. Creator bylines are bounded and shortened
at a UTF-8 boundary. Browser results render metadata as text, and a chosen title
is shortened to fit the existing source draft. Cancelling or changing a search
retires its pending response. Review and confirmation still recheck the chosen
destination before a playback command.

The read permission is `search_spotify` or the policy's `read` class. Denied
requests do not call Spotify. Missing sign-in returns `SPOTIFY_AUTH_REQUIRED`;
invalid requests return `INVALID_ARGUMENT`; cooldown, unavailable catalog and
malformed provider results return `NOT_READY`. Non-advancing or inconsistent
continuations are rejected before sources are offered. A custom playback-only
provider that has not implemented search returns `NOT_IMPLEMENTED`; a legacy
search provider still serves its first page and refuses later offsets.

The existing `fsonos search`, `/library/search`, and `search_library` continue
to search the saved local library and optional Sonos favorites. Catalog search
does not write cache rows, change DJ preferences, or filter results through
the DJ's standing taste.

## Validation coverage

The regression sources cover all six kinds; exact query/type binding; bounded
and malformed results; policy denial before I/O; token refresh, API refusal and
shared cooldown; direct CLI, HTTP, remote CLI and standalone MCP; and explicit
browser and native find → choose → review → play/queue flows. Native tests use
controlled daemon replies and cover both household generations, all six kinds,
all three actions, lifecycle cancellation, changed destinations and single-use
confirmation. They do not require a live Spotify account or physical speakers.

Focused Rust commands:

~~~bash
cargo test -p fsonos-spotify catalog_search
cargo test -p fsonos-api --test spotify_search
cargo test -p fsonos-mcp tools::spotify_search
cargo test -p fsonos-cli spotify_search_cmd
cargo test -p fsonos-cli spotify_sources
cargo test -p fsonos-cli --test e2e_spotify_search
~~~

Focused browser command:

~~~bash
node --test scripts/test-remote-source-player.cjs scripts/test-dashboard-source.cjs \
  scripts/test-phone-catalog-basket.cjs scripts/test-dashboard-catalog-basket.cjs
~~~

Pagination regressions cover filtered and duplicate raw positions, empty filtered
pages with continuation, the provider offset bound, later-page counter mismatches,
HTTP and CLI parameter validation, and MCP forwarding that refuses a stale
first-page reply.

The browser tests cover explicit next/previous navigation, legacy first-page
replies, query and kind changes, cancellation, stale item selection, and bounded
raw progress. Phone and dashboard DOM-fixture tests exercise their actual paging
buttons and verify that browsing pages only reads catalog data, without preparing
a playback action or sending speaker writes.

The browser basket suites also cover mixed tracks and episodes from multiple
searches, reordered repeats, all three actions and multi-household destinations,
exact original-page verification, stale controls, changed audiences and queues,
partial receipts, lost replies and slow provider responses. They run the shared
model, shipped controllers and DOM controls under Node.

Focused native command:

~~~bash
swift test --package-path apps/apple --filter SpotifySearch
~~~

Native pagination regression sources cover request and response bounds, exact
query/category/position matching, legacy first-page replies, raw previous/next
history, empty filtered pages, stale replies, cancellation during a pending page,
and selected-daemon reads that retain the explicitly chosen later-page source.

These fixture-based checks do not establish live Spotify or physical-speaker
acceptance.
