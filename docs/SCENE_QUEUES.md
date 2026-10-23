# Recall a saved queue in a scene

By default, saving a scene while a normal Sonos queue is selected records the queue's
selected track occurrence, elapsed position and known shuffle/repeat mode.
The original queue remains on its speaker. Applying the scene can select it
again after that room has switched to radio or another direct source.

```bash
# With the desired queue and track selected:
fsonos scene save evening

# Later, after listening to a radio source:
fsonos scene apply evening
```

The existing HTTP capture (`PUT /scenes/evening` with `{}`), MCP `save_scene`
without an authored definition, and capture controls use the same behavior.
The web remotes and Apple client display the saved track number and time and
explain the unchanged-queue requirement. Capturing remains a read of the house
followed by a local storage write; it does not change playback.

## Back up queue contents for durable recall

Use `--backup-queues` when the scene should survive edits or a restart of its
original speaker:

```bash
fsonos scene save evening --backup-queues
# The queue may subsequently be edited or replaced.
fsonos scene apply evening
```

The HTTP equivalent is `PUT /scenes/evening` with
`{"backup_queues": true}`. MCP `save_scene` accepts `backup_queues: true`
without an authored `scene`; the web scene manager offers **Back up queue
contents**. These options capture the house as it is now and cannot accompany
an authored definition or CLI `--from` file.

Every ordinary selected queue is saved completely at one observed revision,
including ordered occurrences, duplicate tracks, native replay metadata,
selected occurrence, elapsed position, and known play mode. The limits are
**1,000 entries and 4 MiB of URI plus metadata per queue**. An incomplete,
changing, oversized or unplayable queue fails the capture before replacing the
saved scene. Capture does not write to any speaker. DJ groups retain their DJ
source, as in ordinary scene capture.

Backups use source `kind: queue_backup`. The public nested `queue` object has
the same summary fields as a bookmark plus `total`, the number of saved
occurrences. Raw entry URIs and replay metadata stay in the scene store and are
not exposed in the scene summary. The backup belongs to its original player
UUID and household; a new player with the same room name cannot inherit it.

Recall establishes fresh queue, boot and source guards on that player. It
stages the saved entries at the current queue's tail, checks their complete
ordered contents and the untouched original prefix, then removes the old
prefix using the speaker's queue revision. It never clears the queue before
staging. Finally it restores the selected occurrence, time, known mode and
playing intent; a paused scene never sends `Play`.

If another controller changes the queue, source, group, boot epoch or playback
position during restoration, the operation refuses to continue when ownership
cannot be established. Uncertain append or removal replies require read-back
proof and are never replayed. A failure may leave staged entries appended to
the current queue; inspect the result before retrying. A failed queue source
blocks subsequent playback for that scene group. Independent groups and room
settings can still apply.

**Recall replaces the original player's current queue contents.** Existing
action-history undo can attempt to restore speaker state, but it cannot
reconstruct the queue that was replaced. Both the caller and scene saver need
`play_queue`, `add_to_queue`, `remove_from_queue`, `pause`, and `seek`, plus
`set_play_mode` for a known saved mode and `play` if the scene starts playback.

The remaining sections describe the default bookmark capture and recall.

## What is retained

Each captured queue source records:

| Field | Meaning |
|---|---|
| `coordinator` | The original speaker identity, independent of its room name. |
| `household` | Its household identity, when known. |
| `update_id` | The queue revision observed during capture. |
| `track` | The original one-based queue occurrence, preserving duplicate-track identity. |
| `position_secs` | The elapsed position to recall within that occurrence. |
| `play_mode` | The known Sonos shuffle/repeat mode, or absent when unavailable. |
| `boot_seq` | The speaker's boot sequence, when reported. |

These fields appear in the nested `queue` object of a scene source whose
`kind` is `queue`. The scene group retains its desired playing or paused state.
An active DJ is still captured as a DJ source; its future music is chosen by
the DJ when the scene starts it.

Queue references are created by capturing the house. An authored JSON/TOML
definition or MCP argument cannot invent one. Use a Spotify track, album or
playlist URI, a favorite, a DJ source, or `keep` for authored music routines.

## When recall succeeds

The original speaker must still be the scene's resolved coordinator in its
original household, with the same queue revision. If a boot sequence was
captured, it must still match. A replacement speaker with the same room name
cannot inherit the reference.

Recall selects the queue, restores the saved mode when known, seeks to the
saved occurrence and elapsed position, and follows the group's saved playback
intent. A paused scene stays silent. An already matching state needs no
playback changes. The operation does not enqueue, remove, reorder or clear
queue entries.

The engine checks the queue and target throughout restoration. A failed source
or position step blocks later playback; it cannot fall through to resuming the
radio or another source that happens to be selected. An uncertain source write
is reconciled through the existing snapshot restore path.

## Changed queues and partial results

Editing, replacing or clearing the queue invalidates its saved reference.
Moving it to another coordinator also invalidates recall, as does a changed
reported boot sequence. Apply reports that source step as failed. Independent
groups and room settings can still apply, following the existing scene
dependency rules. Inspect `complete`, `failed`, `skipped` and policy notes in
the apply result; scene application is not atomic.

The default saved reference contains no queue backup. Use `--backup-queues`
when capturing to retain the entries, author a scene with an exact Spotify URI
for a reusable source, or capture a new bookmark after queue changes.

Existing scenes stored with `kind: keep` continue to retain the source that
the room has at apply time. Capture them again to record a queue reference.

## Policy and undo

Both the caller and the scene's saver must be permitted to select the queue,
seek, restore a known play mode, and perform the final playback action.
The same scene volume limits and grouping checks continue to apply.

Recall leaves queue contents unchanged, so the existing action snapshot can
restore the intervening radio source or the previous position in that same
queue. `fsonos undo` retains its normal freshness and policy checks and reports
any aspect it cannot restore.
