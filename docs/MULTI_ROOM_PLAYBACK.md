# Play or queue a source across existing groups

`play`, `play_tracks`, `play_favorite` and `play_sonos_playlist` accept the same room sets as
transport and volume controls. A room name still selects its current group.
A multi-room alias, `all` or `everywhere` selects every distinct group named by
that target. `all@S1` and `all@S2` restrict the selection to one household.

```bash
fsonos rooms alias add downstairs Kitchen "Living Room"
fsonos play downstairs 'spotify:album:<album-id>'
fsonos play all@S2 --favorite 'Morning'
fsonos play everywhere --playlist 'Evening Mix'
fsonos play downstairs 'spotify:album:<album-id>' --next
fsonos play all@S2 --playlist 'Evening Mix' --queue
fsonos undo
```

The aliases and room names above are examples. Existing groups stay together;
two rooms that already share a coordinator cause only one playback command.
Separate groups start independently. This operation does not provide
synchronized playback across groups or join S1 and S2 households. Use `party`
to group one household before playback when that is the intended arrangement.

## Shared interfaces

| Operation | CLI | HTTP | MCP |
|---|---|---|---|
| Spotify or stream source | `play <target> <source>` | `POST /play` | `play` |
| Ordered tracks or episodes | `play-tracks <target> <URI>...` | `POST /play/tracks` | `play_tracks` |
| Sonos favorite | `play <target> --favorite <query>` | `POST /play/favorite` | `play_favorite` |
| Saved Sonos playlist | `play <target> --playlist <query>` | `POST /sonos/playlists/play` | `play_sonos_playlist` |
| Add a source next or at the end | `play <target> <source> --next` / `--queue` (also `--favorite` / `--playlist`) | `POST /queue` | `add_to_queue` |

The target remains `zone`, and the outcome retains `done`, `changed` and
`notes`. A request that plans several control commands also returns a `batch`
receipt; single-group playback retains its existing response shape. Direct CLI,
daemon CLI, standalone MCP and daemon MCP use the shared planner and executor.
The phone remote and desktop dashboard use the reviewed source endpoint below
for room sets and favorite-title selections. It retains the same underlying
source preparation, policy tools, action logging and per-group receipts.

Ordered playback sends the same 1–10 tracks or episodes to each group,
preserving order and intentional repeats. It requires no Spotify Web API lookup
for concrete track identities. An optional `expected_revision` queue token
requires a selector resolving to one distinct group; one group's token cannot
be used for a whole room set. See [ordered track selections](QUEUE_TRACK_SELECTIONS.md).

Search-and-play picks, line-in playback, ordered queue additions
(`queue-tracks` / `POST /queue/tracks`), the browser's ordered-track review, and
the legacy `/sonos` compatibility bridge retain their single-group contracts. They do not silently choose one room
from a multi-room alias.

## Phone and desktop controls

Open the existing source player and change **Destination selection** to a room
set. Enter a room, alias, `here`, `all@S1`, `all@S2`, or `everywhere`, or read
the available room suggestions. Suggestions do not determine membership: the
daemon resolves the actual player identities and complete affected groups.
Choose a source link, a Spotify search result, a favorite title or a saved
playlist title, then **Review playback** or **Review queue addition**.

The review lists every affected group, its current music and relevant queue
position. Group members outside the named alias are included. Favorites and
playlists use an unambiguous full title in each household; the review shows
each household's own entry and ID. The browser does not copy an ID from one
household into another household's selection. The single-group playlist picker,
individual saved-track actions and line-in controls remain available.

Confirmation checks the complete target, catalog selection and relevant state
again, reserves the existing browser command lanes, and sends one daemon
request containing the resolver's revision. The daemon checks that revision
against the same room snapshot it uses to plan the action. Each queue receipt
must account for every reviewed group. Successful
ranges, failed destinations, partial completion and undo limitations stay
visible, including an all-failed batch. A missing or malformed reply is
uncertain and is never automatically repeated. Source panels add no background
polling. The review cannot lock out another controller after its final read;
the daemon's execution checks still apply.

## Reviewed source requests

`GET /rooms/resolve?target=<target>` returns an opaque `revision` covering
the selected rooms, complete physical groups, coordinators and known addresses.
After reviewing that observation, send the exact target and revision to
`POST /sources/control`:

```json
{
  "zone": "downstairs",
  "expected_revision": "<revision returned for downstairs>",
  "mode": "next",
  "source": {
    "kind": "uri",
    "source_uri": "spotify:album:<album-id>",
    "title": "Evening listening"
  }
}
```

The mode is `play`, `next` or `append`. The source is exactly one of
`{"kind":"uri","source_uri":"...","title":"..."}`,
`{"kind":"favorite","favorite":"Morning"}` or
`{"kind":"playlist","playlist":"Evening Mix"}`; `title` is optional for
URI sources. Streams support `play` only. Unknown fields, unsupported modes
and malformed revisions are rejected before discovery or speaker reads.

A revision that no longer matches the planning snapshot returns
`NOT_COORDINATOR` before source preparation or speaker writes. A target that
no longer resolves keeps its normal resolution error. After admission the
concrete original destinations remain fixed, even if an alias changes during
a slow catalog read. Every subsequent speaker write checks the original
physical group, room names and bonding again. A new member, renamed or
rebonded room, changed coordinator or changed address causes a refusal; the
request does not heal onto another group or expand its audience. This also
applies to aliases resolving to just one group.

Play uses the ordinary `play` or `play_favorite` policy permission and
returns the existing playback outcome. Queue modes use `add_to_queue`
permission and return the full `added`/`failed` receipt described below,
including when only one group was reviewed. If some speaker steps already
went out before a later failure, the result preserves that uncertainty and
does not advertise aggregate undo. Inspect the affected group before another
action; no whole-request retry is performed.

The phone and dashboard keep the revision in both the review comparison and
the submitted body, so physical changes that leave all displayed names
unchanged still retire the review. A daemon that omits a valid revision
cannot accept a room-set source review in these controls. The ordinary source
endpoints remain available to clients using their existing immediate-command
contracts.

## Preparing the source

Every selected household uses its own Spotify renderer or native source
metadata. Favorites and saved playlists are looked up once per household,
then reused for its selected groups. Titles, numbered positions and native
ids are resolved independently in those catalogs. An identical `SQ:` or
`FV:2/` id on two households need not refer to the same music; a shared title
is usually the clearer choice when intentionally spanning households.

Invalid, missing or ambiguous native sources and missing Spotify linkage are
reported before any group starts, its levels change or its fade is cancelled.
Preparation reads do not make speaker state atomic; execution still handles
changes and failures that occur afterward.

Artist and show links retain every destination's queue, source and group
observation before the slow catalog read. One lookup supplies the same ordered
selection for all selected groups, and every destination is checked again
before dispatch. Each group renders that selection through its own household.
This preserves the bounded artist/show behavior described in
[Artist and show playback](ARTIST_PLAYBACK.md).

## Failures, policy and undo

The caller's current authorization is checked again after source preparation.
Existing policy caps apply to every affected group and its actual audience.
Disjoint playback groups use the existing bounded concurrent executor, so a
slow player does not serialize all starts.

One speaker-side failure can leave other groups successfully playing. Multi-target
control responses include `batch.complete` and an ordered `batch.results` entry
for every planned command. These receipts also cover ordinary transport, volume,
mute and grouping controls, with the target identity captured before execution:

| Field | Meaning |
|---|---|
| `batch.complete` | Every planned command completed successfully. |
| `batch.results[].household` | The household label, such as `S1` or `S2`. |
| `batch.results[].target` | The originally addressed player UUID; any recovered destination is described in notes. |
| `batch.results[].zone` | That player's room name when the command was planned. |
| `batch.results[].result` | A tagged result: `succeeded` retains `done`, `changed`, optional `volume` and notes; `failed` retains the structured `error`; `not_attempted` means an earlier failure ended sequential dispatch. |

For example, a partial pause can return this receipt (identities are synthetic):

```json
{
  "done": "paused Kitchen",
  "changed": true,
  "notes": [{"code": "SKIPPED", "detail": "Study@S2 skipped: speaker refused; do not replay the batch"}],
  "batch": {
    "complete": false,
    "results": [
      {"household": "S1", "target": "RINCON_EXAMPLE_KITCHEN", "zone": "Kitchen", "result": {"status": "succeeded", "done": "paused Kitchen", "changed": true}},
      {"household": "S2", "target": "RINCON_EXAMPLE_STUDY", "zone": "Study", "result": {"status": "failed", "error": {
        "code": "UPNP_FAULT", "detail": "speaker refused", "upnp_code": 701,
        "hint": "Inspect this target before another command.", "suggestions": [], "retryable": false
      }}}
    ]
  }
}
```

**An HTTP 200 with `batch.complete: false` is partial delivery.** Successful
targets remain in the receipt even when another target has a terminal or
uncertain failure. The CLI prints that receipt (`--json` preserves its full
structure) and exits nonzero. MCP retains the same structured content and sets
`isError: true`. Both direct and daemon-backed modes behave this way; neither
falls back to a second execution. Scheduled controls record an incomplete run
and consume that occurrence without retrying it. Benign clamp, recovery and
logging notes do not turn a successful command into a failure.

The top-level `SKIPPED` notes remain for older clients; clients connected to an
older daemon can use those typed notes when `batch` is absent. A single-target
failure, or a batch in which every attempted command fails, keeps the existing
error response. Read each failed or unattempted target before a new targeted
action. Never replay the whole batch: successful groups are not automatically
rolled back, and an uncertain write may already have taken effect.

The set is recorded as one action. A successful stream change can restore all
captured groups with one undo. Queue replacements require complete before/after
evidence for every attempted replacement; missing proof or a partial replacement
leaves the existing undo barrier. Subsequent manual changes and current policy
continue to limit restoration. See [Queue undo](QUEUE_UNDO.md).

## Queue additions and per-group receipts

`--next` and `--queue` add the source to each selected group's existing queue.
They preserve the current source, volume, shuffle/repeat mode and play/pause
intent. Next means after that group's current queue occurrence; a group playing
another source, or with an empty queue, receives the addition at the end.
Artist/show requests share one catalog selection. Native favorites and saved
playlists retain the metadata from each household's own catalog.

The complete original destination of every group is observed before catalog
reads. Source, group, boot and queue evidence is checked again before dispatch
and inside each insertion engine immediately before its write. A change during
renderer learning or a definite renderer-rejection retry cannot become a new
baseline for the request.

An ordinary `POST /queue` request resolving to one group keeps the existing
response, including when an alias names two members of that group:

```json
{"done":"queued 2 tracks","zone":"Study","first":4,"count":2,"playing_queue":true}
```

An ordinary request resolving to several groups, and every reviewed
`POST /sources/control` queue request, returns a receipt with these fields:

| Field | Meaning |
|---|---|
| `done` | Confirmed group count and a clear incomplete status when any group failed. |
| `complete` | Every selected group returned a confirmed insertion. |
| `added` | Each successful group's `household`, `coordinator` and `queue` object containing its own `zone`, `first`, `count`, `playing_queue` and `done`. |
| `failed` | Each failed group's `household`, `coordinator`, `zone` and structured `error`, including its code and uncertainty. |
| `logged` | The single aggregate action was recorded successfully. |
| `undoable` | That recorded action contains verified ownership for all possible effects; later queue changes can still prevent undo. |

There is no shared `first` position or aggregate queue length. Separate groups
can have different queues and native playlist contents.

Before dispatch, preparation failures return an ordinary error without queue
writes. After dispatch begins, HTTP returns the complete batch receipt, even
if every group failed. **Check `complete` and every `failed` entry; an HTTP 200
does not mean all groups accepted the addition.** The CLI prints the receipt
(JSON with `--json`) and exits nonzero for an incomplete batch. MCP retains the
same structured receipt and names each destination's result in its text.

Never repeat the whole request after partial or uncertain delivery. Read the
failed groups' queues before deciding on a new targeted add. Definite refusals
can leave confirmed additions in the other groups undoable as one action.
Any uncertain insertion creates a barrier for the entire batch: undo cannot
claim to remove only the known ranges while ignoring possible extra tracks.
No response failure triggers a second local execution after the daemon has
answered.

## Validation

Regression coverage exercises both virtual Sonos generations, grouped-room
deduplication, household-local catalogs, shared artist lookup, failed source
preparation, partial speaker failure and restoration. Queue regressions cover
distinct insertion ranges, quiet next/append, native catalogs, uncertain writes
and the retained destination during internal preparation. Actual CLI and MCP
process scenarios are in `crates/fsonos-cli/tests/e2e_source_targets.rs` and
`crates/fsonos-cli/tests/e2e_control_batches.rs`; the latter checks partial
pause delivery, one dispatch per target, nonzero CLI status and MCP error
signaling in direct and daemon-backed modes.

```bash
cargo test --locked -p fsonos-api --test source_targets_sim
cargo test --locked -p fsonos-api --test queue_targets_sim
cargo test --locked -p fsonos-cli --test e2e_source_targets
cargo test --locked -p fsonos-cli --test e2e_control_batches
cargo check --locked --workspace --all-targets
```

Rust execution of this addition remains unverified in this environment: the
workspace check was terminated by SIGKILL while compiling the existing
`asupersync 0.5.0` dependency, before reaching project code. Formatter and diff
checks are separate from runtime validation. Physical-speaker acceptance is
also outstanding.
