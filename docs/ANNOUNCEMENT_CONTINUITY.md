# Announcements that preserve a continuing group

When an announcement targets a room that coordinates a group, but leaves
other rooms in that group unannounced, those other rooms keep the live music.
After the clip, the target rooms return to that continuing transport and the
original coordinator takes over again. The queue, selected occurrence,
elapsed position, shuffle/repeat setting and playing or paused state travel
with the native handoff.

This closes the gap where restoring the original coordinator's old queue
bookmark could rewind the unannounced rooms or fail because its queue had
already moved to a surviving coordinator.

## How the rooms return

1. Cap the target rooms before regrouping or unmuting them. In each partially
   targeted group, detach targeted members before its targeted coordinator,
   so an untargeted room receives the continuing music.
2. Confirm that transfer against the expected physical players, bonded rooms,
   addresses, reported boot sequences, complete ordered queue and playback.
3. Play the announcement in the temporary target group. The surviving groups
   continue independently.
4. Detach the temporary members and retain a playback observation for each
   returning room. Read the survivor's **current** playback, rejoin the target
   rooms, and use `DelegateGroupCoordinationTo` with `RejoinGroup=1` to return
   coordination while retaining the survivor in the group.
5. Verify the final topology, complete queue, selected occurrence, clock,
   transport and known play mode before restoring the target rooms' levels.
   The restored playback and group identity also fence those level changes.

Several partial groups can take part in one announcement. A bonded room
travels with all its present players. Duplicate queue entries retain their
selected slot. A paused or stopped continuing group remains silent after the
clip.

## Current playback wins

The survivor is read again when the rooms return. Queue edits, seeks, mode
changes and pauses made in that continuing group during the clip therefore
survive. Restoring its old snapshot is unnecessary: the controller does not
clear, reconstruct, seek or restart the continuing queue.

A changed group, missing or rebooted player, conflicting identity, newer
source in a released target room, or a playback change during the handoff
prevents subsequent restoration writes. The announcement report records the
incomplete restoration. Music can remain under the surviving coordinator
when returning the original coordinator cannot be proved.

Each handoff is sent once. A lost or malformed reply is accepted only after
fresh topology and playback reads prove the full transfer. Explicit protocol
refusals retain their error; an acknowledged no-op also fails verification.
Acknowledged joins and native transfers have a bounded topology-settle window;
only observations repeat while waiting for their effects.
Neither result triggers a queue replay. If the clip installation itself was
rejected and the original source is confirmed untouched, the rooms and
levels can still return through the native path.

## Evidence and validation

| Native operation | Evidence |
| --- | --- |
| Leaving a coordinator with `RejoinGroup=0` | Existing S1 and S2 hardware observations in [PROTOCOL.md](PROTOCOL.md), section 6 |
| Returning coordination with `RejoinGroup=1` | Simulator behavior and guarded controller implementation; no physical-speaker validation is claimed here |

`crates/fsonos-core/tests/announce_continuity_sim.rs` contains S1 and S2
localhost regressions for continued queue progress, duplicate occurrences,
silent states, multiple partial groups, a bonded coordinator, live edits,
rejected clip installation, lost/malformed native replies, refused or
unapplied handoffs, delayed joins, regrouping, reboot and ownership changes
during rejoin.
Run that target with:

```sh
cargo test -p fsonos-core --test announce_continuity_sim
```
