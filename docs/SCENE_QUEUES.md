# Recall a saved queue in a scene

Saving a scene while a normal Sonos queue is selected records the queue's
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

The saved reference contains no queue backup. To return to an edited playlist
in a reusable routine, author a scene with that playlist's exact Spotify URI.
To recall the current queue state, capture a new scene after the changes.

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
