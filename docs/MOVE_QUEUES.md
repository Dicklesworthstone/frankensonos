# Moving queue playback safely

A normal move within one household uses native group coordination whenever
the players support it. Cross-household transfers with **fsonos move --copy**
and the fallback for unsupported coordination must copy the source queue.
Both now stage the replacement before removing anything already queued at
the destination.

## What a queue transfer does

1. Read the complete source queue and learn the destination household's
   renderer parameters. Keep duplicate occurrences, the selected slot,
   elapsed position, transport intent, and known shuffle/repeat mode.
2. Read the destination's complete queue and verify its current group,
   coordinator boot sequence when available, source, selected occurrence,
   transport state, position, and known mode. Refuse an unreadable or
   oversized combined queue before sending Stop.
3. Stop the reviewed destination without an audible Play/Pause cycle.
   Append incoming occurrences after the existing queue. Each acknowledged
   append is read back; an uncertain append requires the complete expected
   prefix and suffix. No append or Stop is replayed.
4. Read back the entire original prefix and incoming suffix together. Only
   then remove the original prefix once, using the player's nonzero queue
   revision. A lost removal reply is resolved by reads. A write blocked by
   an ownership check cannot be turned into success by another controller
   making a similar edit.
5. Select the committed queue and restore its mode, selected slot and time.
   Start it only if the source was playing. Confirm destination playback,
   then recheck the original source before stopping it.

This retains the destination's original entries when an incoming track is
refused, an append cannot be proven, or the original prefix changes during
staging. Unsupported native range removal fails with the original prefix
still present; there is no clear-all fallback.

## Failure behavior

This is a sequence of speaker operations, not an atomic transaction. After a
failure the destination may be stopped and may contain an appended suffix.
After a confirmed prefix removal, a later playback failure cannot bring the
old entries back automatically. Read the current queue before retrying.

The source is not stopped unless destination playback is confirmed and the
source still matches the playback that was copied. A new destination source,
seek, mode, group, or reboot stops reconstruction when observed. The native
revision check also rejects a queue edit in the final read/write race.
Independent controller actions cannot be made atomic with these reads.

Direct streams and single-source URIs continue to use their existing loading
path. Native same-household handoff does not rebuild the queue.

## Regression coverage

The moving_queue_staging_sim suite exercises S1-to-S2 and S2-to-S1 transfers
through the localhost SOAP simulator: repeated tracks, silent paused-source
loading, lost and malformed acknowledgements, refused or unapplied appends,
a changed prefix of the same length, owner seeks/source/mode changes, ignored
removal, and a native revision race. Existing moving and play-mode suites
continue to cover delegation fallback and release of the source only after
read-back.

Run:

    cargo test -p fsonos-core --test moving_queue_staging_sim
    cargo test -p fsonos-core --test move_play_mode_sim --test moving_sim
    cargo test -p fsonos-core moving::replay
