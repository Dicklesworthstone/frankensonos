# Undo a queue removal or move

Confirmed native queue removals and moves can retain a complete, durable undo
backup. The response's `undoable` field is the authority: it becomes `true`
only after the daemon saves the backup and the exact confirmed result in its
action log. A successful edit remains successful if logging fails, but its
response reports `undoable: false`.

Undo restores the original ordered contents, including duplicate occurrences,
the original selected slot and position, the saved shuffle/repeat mode, and
whether playback should start. A paused queue stays silent. The existing
action log stores this information, so daemon restart does not discard the
backup. Older action records retain their existing meaning.

## Review and restore

The remote's activity view uses the same review flow as other undoable actions:

1. Read `GET /undo/preview` and review the caller's newest eligible action.
2. Send its exact `id` as `{"expected_id": <id>}` to `POST /undo/checked`.
3. Inspect the returned `failures`, if present. A successful HTTP response can
   describe a partial restore; it does not promise every speaker write worked.

The CLI's `fsonos undo`, MCP `undo_last`, and legacy `POST /undo` use the same
queue recovery. The normal one-hour undo age limit and caller policy apply.
Neither a preview nor `undoable: true` reserves the speaker state indefinitely.

## Which edits can be recovered

The original queue must be nonempty, selected as the coordinator's own queue,
fully readable, and within both backup limits: **1,000 entries and 4 MiB of
serialized queue metadata**. Each entry must have replayable metadata. A
complete backup is taken before the native edit and bound to that exact
revision and requested range. After the edit, the daemon proves the complete
expected order at the native operation's confirmed revision before saving it.

An inactive queue, an unselected or initially empty queue, an oversized or
unreadable queue, and an unconfirmed edit do not gain a partial backup. Their
changed or uncertain actions remain barriers, so undo cannot skip past them
and restore a stale earlier queue reference. No-op moves create no barrier.
Deleting every entry from a previously selected eligible queue can be undone.

This feature covers native removal and reordering. It does not add content
backup to playback replacement or every operation that changes a queue.

## A later listener's changes take precedence

Before any restore writes, the daemon checks the saved post-edit queue
revision and every ordered renderer URI, selected occurrence, source metadata,
transport state, position, duration, play mode, room volume/mute, physical
membership, addresses, and available boot identities. Natural forward progress
within the same playing occurrence is allowed. A changed track, seek, source,
pause/resume, mode, queue, group, reboot, or room level prevents recovery.
`--force` does not bypass these queue ownership checks.

The restoring caller must be allowed to pause, add and remove queue entries,
select the queue and seek; restoring a known play mode also requires that
permission. An audible restore requires resume and volume permissions and
applies current policy caps before any playback can start. Recovery never
raises room volume back to an older setting. Quiet restoration does not
require permission to resume or set volume.

## Reconstruction and uncertainty

The daemon pauses as needed, appends and verifies the complete saved contents,
then removes the old prefix using its exact native revision. It does not clear
the queue first. Only after successful reconstruction does it restore the
selected occurrence, position, mode and play intent. Queue and playback checks
remain active at write boundaries, with physical membership and room-level
checks through the final playback command.

A lost append or removal reply is reconciled by reads; the write is never
blindly repeated. If reconstruction fails after staging begins, a verified
suffix may remain in the paused queue. The result explains the failure; no
automatic rollback or stale reference-only restore follows it. Read the
speaker queue before deciding what to do next. Undo itself has no redo.

Regression sources cover S1/S2 localhost speakers, duplicate and current-entry
removal, complete deletion, reordering, paused and advancing playback, SQLite
close/reopen, changed ownership, policy caps, lost replies and partial staging.
These fixtures are separate from physical-speaker qualification; check the
matching CI results for executed Rust validation.
