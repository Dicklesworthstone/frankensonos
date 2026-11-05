# Sleep fades across daemon restarts

The daemon fades a sleep timer's group during its last two minutes, pauses it,
then restores each room's original volume so the next playback starts at the
expected level. Each speaker's own timer remains a backstop five minutes after
the daemon's deadline. A direct CLI or standalone MCP timer uses that native
timer without the daemon's fade.

## Recovering an interrupted fade

The daemon keeps both armed timers and unfinished fades in its private
`sleep-timers.json`. Before a fade can change a speaker, an atomic, synced
checkpoint records every affected room's original volume, last confirmed
volume and next intended volume. Keeping both confirmed and pending levels
covers a process stop before a write and a lost acknowledgement after a write.
Earlier saved files containing only timers remain readable.

On restart, the daemon verifies the recorded physical group, bonding and
available boot identities, the playback source and the speaker's timer. A
queue must still have the same known, nonzero queue revision. Recovery follows
the evidence:

- A matching backstop, unchanged playback and still-owned room levels allow
  the fade to continue toward its original deadline. Completion restores the
  original levels from before the first fade, rather than treating attenuated
  levels as a new baseline.
- A fade already entering its final stop or volume cleanup can only finish
  volume cleanup. Recovery does not repeat an uncertain pause, start playback,
  seek, edit the queue or rebuild a group.
- A room whose volume differs from the fade's confirmed and pending values
  keeps that newer level. Other still-owned room levels can be restored, and
  the timer's matching native backstop is retired before cleanup.
- A changed source or physical group retires the old recovery without a
  volume write. Missing or failed reads keep its evidence for another attempt.

Cancellation, extension and replacement keep unfinished cleanup separate from
the timer definition. A replacement fade waits until its predecessor's cleanup
settles, so it cannot capture an attenuated level as its starting volume. A
newly armed timer retains its own native backstop.

Accepted local volume and playback changes fence recovery reads already in
flight. That includes a change while the first fade snapshot is still being
read: the obsolete due timer is retired from the saved file before the newer
command reaches a speaker. Ordinary future timers retain their existing policy.

## When recovery cannot prove ownership

If the initial checkpoint cannot be published, the daemon sends no fade
volume writes and leaves the speaker's backstop in place. An unreadable or
malformed saved file is preserved rather than replaced with an empty state.
Read failures remain pending across subsequent saves and restarts.

Matching levels and source identity are observed evidence, not a lock on other
controllers. An external change that returns to exactly the same observed
state cannot always be distinguished. A changed or uncertain group is never
permission to restore a recorded volume into another group. If the daemon is
unavailable, the native backstop can pause playback but cannot perform the
daemon's volume restoration; cleanup awaits a running daemon and readable,
matching state.

## Regression coverage

`cargo test -p fsonos-core --test sleep_recovery_sim` exercises simulator writes
interrupted before and after acknowledgement, resumption with original levels,
an accepted pause, manual volume overrides, changed sources/groups, and failed
checkpoints. Core sleep dispatch tests also cover a worker waiting on a room
step while a checkpoint reads the timer map and nonblocking timer adoption.

`cargo test -p fsonos-api --lib surface::schedules::recovery` covers real saved
files, S1/S2 restart recovery, unreadable speakers, cancellation/replacement,
failed publication, worker ownership handoff, and manual changes during both
initial capture and restart verification. These use synthetic loopback devices;
physical-speaker and tailnet acceptance remain separate.
