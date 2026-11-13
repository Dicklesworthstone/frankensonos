# Undo queue edits, replacement playback and scenes

Confirmed native queue removals, moves and eligible playback replacements can
retain a complete, durable undo backup. Native removal and move responses report
`undoable: true` only after the daemon saves the backup and exact confirmed
result in its action log. Playback commands keep their existing response shape;
use action history and the undo preview to inspect recovery eligibility. A
successful speaker operation remains successful if logging fails, but cannot
promise durable recovery without its saved action.

Undo restores the original ordered contents, including duplicate occurrences,
the original selected slot and position, the saved shuffle/repeat mode, and
whether playback should start. A paused queue stays silent. The existing
action log stores this information, so daemon restart does not discard the
backup. Older removal and replacement records retain their existing meaning.

## Review and restore

The remote's activity view uses the same review flow as other undoable actions:

1. Read `GET /undo/preview` and review the caller's newest eligible action.
2. Send its exact `id` as `{"expected_id": <id>}` to `POST /undo/checked`.
3. Inspect the returned `failures`, if present. A successful HTTP response can
   describe a partial restore; it does not promise every speaker write worked.

The CLI's `fsonos undo`, MCP `undo_last`, and legacy `POST /undo` use the same
queue recovery. The normal one-hour undo age limit and caller policy apply.
Neither a preview nor `undoable: true` reserves the speaker state indefinitely.

## Queue additions

Confirmed additions retain their exact original and resulting queue observations,
coordinator address, physical group, selected source and known speaker boot.
This covers single tracks, ordered selections, service containers and native
favorites or saved playlists. Undo compares the saved revision and complete
ordered renderer URIs before sending one revision-guarded native removal. An
external replacement with identical tracks and positions does not authorize
removing those newer occurrences. `--force` does not bypass this proof.

Receipts require one-step native revision advances for each addition. A verified
undo can record its own one-step removal and restored revision, allowing earlier
additions to be undone in succession, including after a daemon restart. Only
those logged transitions can advance an older receipt; equal-looking contents,
skipped counters, a reboot or an unconfirmed removal cannot. Transition lookup
is bounded to the most recent 256 action rows; missing evidence leaves the queue
intact.

Proofs are bounded to 1,000 entries per queue and 4 MiB of serialized action
state. A successful addition with unknown boot/address evidence, an oversized
receipt, a wildcard result revision, a wrapped counter, or other counter behavior remains
successful but creates an undo barrier. Historical additions without a retained
receipt remain readable and cannot authorize removal, including under force.
Use action history and the undo preview to inspect recovery eligibility.

## Which actions can be recovered

The original queue must be nonempty, selected as the coordinator's own queue,
fully readable, and within both backup limits: **1,000 entries and 4 MiB of
serialized queue metadata**. Each entry must have replayable metadata. A
complete backup is taken before the native operation and bound to that exact
revision. For a removal or move, it also binds the requested range and resulting
selected occurrence. After the edit, the daemon proves the complete expected
order at the native operation's confirmed revision before saving it.

Playback replacement covers Spotify albums and playlists, resolved artist-track
and show-episode selections, native Sonos playlists, and container favorites.
The replacement must finish successfully and start its confirmed new queue.
Its receipt binds the original ordered queue and playback to the staged append,
prefix removal, and exact final revision and contents. The recorded result must
also fit within 1,000 entries and 4 MiB of renderer URI text. This
allows a short old queue to be recovered after playing a longer album.

Caps applied before playback are retained as exact acknowledged reductions.
Undo does not restore a higher pre-cap volume. A multi-group command requires a
complete replacement proof for every attempted replacement; partial results
cannot expose a partial queue restore as an undoable whole action.

An inactive queue, an unselected or initially empty queue, an oversized or
unreadable queue, and an unconfirmed edit do not gain a partial backup. Their
changed or uncertain actions remain barriers, so undo cannot skip past them
and restore a stale earlier queue reference. No-op moves create no barrier.
Deleting every entry from a previously selected eligible queue can be undone.

## Queue-replacing scenes

A scene can retain the same complete queue recovery for Spotify albums and
playlists, resolved artist tracks and show episodes, native Sonos playlists,
and container favorites. Both playing and silent scene loads are eligible.
Every affected group must keep its original physical membership, and every
scene step must complete. Each replaced queue must have its own complete
backup and confirmed replacement receipt; otherwise the entire action remains
an undo barrier.

Scene undo also restores the volume and mute fields that the scene deliberately
changed in those groups. Its baseline is captured after policy cap reductions,
so undo keeps those reductions and applies the restoring caller's current caps.
The original queue is reconstructed and verified in silence before any room
level is raised or a room is unmuted. Playback resumes only if the original
queue was playing. A paused or stopped original queue stays silent.

Completed volume fades can retain recovery. A manual fade takeover, failed
step, missing receipt, or changed final state prevents it, even if the eventual
levels happen to match the requested scene. Scenes that regroup rooms, start
the DJ, or recall a durable queue backup remain outside this recovery scope.
Initially empty, inactive, unreadable or oversized original queues still cannot
be recovered. The apply response reports when replaced contents cannot be
restored; use the action history and undo preview to inspect eligibility.

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
applies current policy caps before any playback can start. Direct replacement
and native edit recovery preserve room levels apart from required reductions.
Scene recovery separately checks permission for its saved volume and mute
fields, including when the original queue was silent; it never needs resume
permission merely to rebuild a quiet queue.

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
removal, complete deletion, reordering, album/playlist and ordered catalog
replacement, playing and silent scene loads, completed and superseded fades,
scoped volume/mute recovery, paused and advancing playback, SQLite close/reopen, changed
ownership, policy caps, lost replies and partial staging. Surface regressions
also exercise the shared checked-undo implementation under HTTP and MCP caller
identities.
These fixtures are separate from physical-speaker qualification; check the
matching CI results for executed Rust validation.
