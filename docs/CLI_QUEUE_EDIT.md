# Edit a queue from the CLI

`queue-remove` and `queue-move` use the same revision-checked actions as HTTP
and MCP. Read the queue first. Neither command reconstructs it or silently
selects replacement entries after a conflict. Both commands require the
revision and exact renderer identities from one observation. After an edit,
read a fresh snapshot before another action; the old revision is intentionally
stale. The examples below are alternatives, not a sequence of edits.

## Review a saved queue page

```sh
fsonos queue 'Room@S2' --offset 0 --limit 100 --json > queue.json
# Inspect queue.json and choose the entries, using their one-based positions.
fsonos queue-remove 'Room@S2' 3 --count 2 --snapshot queue.json
```

This removes positions 3 and 4 as they appeared in that snapshot. Removal is
not undoable. Removing the current entry may change or stop playback. The
command itself is the confirmation; it does not ask an interactive question.
A snapshot is read only, never overwritten, and should be kept out of git
because it contains the household's music and renderer information.

Move those same two entries before original position 1 instead:

```sh
fsonos queue-move 'Room@S2' 3 1 --count 2 --snapshot queue.json
```

Both positions refer to the ORIGINAL queue, before moving the block. To
append, use the snapshot's `total + 1` as the destination. Do not subtract the
block's size. Moving before a position inside the selected block, or just
after it, is a no-op. Reordering is not undoable by `fsonos undo`.

The saved page must include every selected entry. `--count` defaults to one
and is limited to 100. Large queues can be read with `--offset` and `--limit`;
positions in the result stay absolute. `--snapshot -` reads the same document
from stdin. Snapshot input is capped at 1 MiB, and missing revisions, missing
selected renderer URIs, inconsistent positions and incomplete ranges fail
before probing a daemon or contacting speakers. The command's room argument
remains the destination: the snapshot's display-only coordinator name never
overrides it. The backend revision binds the actual coordinator and queue.

## Play an entry from the same reviewed page

`queue-play` accepts the same saved format and the same bounded stdin input:

```sh
fsonos queue-play 'Room@S2' 3 --snapshot queue.json
```

This selects position 3 only when BOTH its exact renderer URI and its queue
revision still match. The CLI reads and validates the file before backend
routing, using the same parser as removal and movement. The requested room
remains authoritative, not the file's display name. A stale, partial, missing,
unreadable or oversized snapshot is never converted into unguarded playback.
Snapshot input conflicts with `--expected-uri` and `--expected-revision`.

Without `--snapshot`, `queue-play ROOM POSITION` keeps its previous behavior:
play the position that exists now. Its individual explicit guards remain
optional for compatibility. Saved snapshots instead always bind both guards.
Playback uses the existing snapshot-undo path and never removes queue entries.

## Explicit renderer identities

Scripts can supply identities directly instead of reading a file:

```sh
fsonos queue-remove 'Room@S2' 3 \
  --expected-uri '<exact URI at position 3>' \
  --expected-uri '<exact URI at position 4>' \
  --expected-revision '<revision from that same queue response>'
```

Repeat `--expected-uri` once per consecutive entry, in queue order. Quote the
values in the shell. Duplicates, commas, escaping and whitespace are preserved;
there is no URI normalization or comma splitting. `--snapshot` cannot be mixed
with explicit identities or revisions, and `--count` is only for snapshots.
The explicit form of `queue-move` also takes the original destination position.

## Routing and uncertain outcomes

Both commands accept the usual global `--json`, `--daemon`, `--daemon-url` and
`--direct` options. Automatic mode uses the daemon when available, otherwise
the direct shared action layer. A dispatched daemon error is propagated; it
never causes the same edit to be attempted locally. Direct mode retains the
shared action log. Native speaker writes remain revision checked, with the
existing read-back recovery for lost replies. Read the queue again after an
uncertain result or stale selection; do not automatically retry an edit.

Regression tests cover parser boundaries, exact duplicate identities, bounded
saved input, partial pages, member-to-coordinator routing, S1/S2 direct edits,
daemon edits, stale snapshots, native no-ops and action logging. Run:

```sh
cargo test -p fsonos-cli --bin fsonos queue_edit_cmd
cargo test -p fsonos-cli --bin fsonos queue_play_cmd
cargo test -p fsonos-cli --test e2e_queue_edit
```

These tests use the virtual-household harness, not physical Sonos hardware.
A committed test is not evidence of a passing run; inspect a recorded verifying
run or run the commands above.
