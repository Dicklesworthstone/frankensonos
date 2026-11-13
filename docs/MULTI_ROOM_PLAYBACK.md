# Play or queue a source across existing groups

`play`, `play_favorite` and `play_sonos_playlist` accept the same room sets as
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
| Sonos favorite | `play <target> --favorite <query>` | `POST /play/favorite` | `play_favorite` |
| Saved Sonos playlist | `play <target> --playlist <query>` | `POST /sonos/playlists/play` | `play_sonos_playlist` |
| Add a source next or at the end | `play <target> <source> --next` / `--queue` (also `--favorite` / `--playlist`) | `POST /queue` | `add_to_queue` |

Playback request and response shapes are unchanged: the target remains
`zone`, and the outcome contains `done`, `changed` and `notes`. Direct CLI,
daemon CLI, standalone MCP and daemon MCP use the shared planner and executor.
The browser remotes continue to choose one displayed group.

Search-and-play picks, line-in playback, revision-bound ordered track
selections (`queue-tracks` / `POST /queue/tracks`), and the legacy `/sonos` compatibility bridge
retain their single-group contracts. They do not silently choose one room
from a multi-room alias.

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

One speaker-side failure can leave other groups successfully playing. Inspect
`notes` for `SKIPPED` entries and read the affected groups before taking a new
action. When every group fails, the request returns an error. The system does
not automatically roll back successful groups or replay an uncertain queue
replacement.

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

A request resolving to one group keeps the existing response, including when
an alias names two members of that group:

```json
{"done":"queued 2 tracks","zone":"Study","first":4,"count":2,"playing_queue":true}
```

A request resolving to several groups returns a receipt with these fields:

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
process scenarios are in `crates/fsonos-cli/tests/e2e_source_targets.rs`.

```bash
cargo test --locked -p fsonos-api --test source_targets_sim
cargo test --locked -p fsonos-api --test queue_targets_sim
cargo test --locked -p fsonos-cli --test e2e_source_targets
cargo check --locked --workspace --all-targets
```

Rust execution of this addition remains unverified in this environment: the
workspace check was terminated by SIGKILL while compiling the existing
`asupersync 0.5.0` dependency, before reaching project code. Formatter and diff
checks are separate from runtime validation. Physical-speaker acceptance is
also outstanding.
