# Spotify catalog search

Find music or podcasts beyond the saved library, choose a result, then play it
through the same guarded Sonos controls used for pasted Spotify links.

## Find and choose

The phone remote and dashboard source selector offer **Spotify catalog**.
Enter a query, choose one content kind, and press **Search**. Typing does not
send requests. Select a result to put its canonical source into the link draft,
then review the target group and **Play now**, **Play next**, or **Add to queue**.
Selecting a result alone never writes to a speaker.

### Native iPhone and Mac app

Open **Spotify online** in the **Music** library panel. Choose one of the same six categories,
enter a query and press **Search Spotify**. The native client uses the daemon's
existing account; it can search without a selected speaker and does not create a
separate Spotify sign-in. Ordinary local reads retain their usual deadline,
while this online search allows 60 seconds for the daemon's 45-second worker.

Results retain the exact query, kind, canonical source URI and observation time.
The first page contains at most ten results; a `more` response asks the user to
refine the query. Search is explicit and single-flight. Changing the query or
category, cancelling, leaving the view or suspending the app retires selection
authority. A cancelled request keeps its slot until it drains, and late replies
cannot overwrite a newer draft.

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
`kind`. The optional limit defaults to ten. The six kinds are `track`, `album`,
`artist`, `playlist`, `show`, and `episode`.

The response contains the normalized `query`, `kind`, ordered `items`, and
`more`. Each item contains `source_uri`, `title`, and an optional `subtitle`.
Sources are canonical Spotify URIs with the requested kind. An empty `items`
array is a successful search with no available matches on that page.
`more: true` means more provider results exist; refine the query to narrow the
selection. It does not provide a continuation token.

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
not contain control characters. Limits are 1–10. Each request reads only the
first provider page; returned next-page URLs and artwork are never fetched.
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
malformed provider results return `NOT_READY`. A custom playback-only provider
that has not implemented search returns `NOT_IMPLEMENTED`.

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

Validation during implementation: 247 source-controller, queue, browser-request
and recovery tests passed under Node, including all 49 new catalog controller
cases and regression coverage for slow search responses, cancellation and the
60-second browser deadline. The pinned workspace formatting check passed.

The Rust commands above remain verification targets, not a claim that Rust tests
passed. Both local Cargo-check attempts were killed while compiling dependencies;
the recovered attempt stopped in asupersync 0.5.0 before project typechecking.
No physical-speaker or live Spotify acceptance is claimed by the browser tests.
