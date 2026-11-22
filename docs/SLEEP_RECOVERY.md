# Sleep fades across daemon restarts

The daemon fades a sleep timer's group during its last two minutes, pauses it,
then restores each room's original volume so the next playback starts at the
expected level. Each speaker's own timer remains a backstop five minutes after
the daemon's deadline. A direct CLI or standalone MCP timer uses that native
timer without the daemon's fade.

The same behavior applies independently to every group selected by a saved
room alias, `all` / `everywhere`, or `all@S1` / `all@S2`. See
[Multi-room bedtime](MULTI_ROOM_BEDTIME.md) for set, extend, cancel, read and
faded-pause commands, browser controls and partial-result handling.

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

## Changing a daemon timer

Timer edits first verify that the addressed endpoint is still the intended
group coordinator. A known address or coordinator mismatch is a target-change
refusal; unavailable or incoherent topology keeps its original protocol error.
Neither case sends a native timer write or retires an existing armed timer.
Native backstop cleanup also verifies the endpoint before clearing it, including
announcement cleanup after a failed rearm. An old address now occupied by another
coordinator never grants permission to clear that coordinator's timer.

After a sleep fade successfully makes a joined room leave, cleanup can use the
same household's witnesses when the leaving player still reports its old group.
Its coherent self-report must still bind the intended player to the original
address. A witness must then bind both its own physical identity and the intended
coordinator to their exact addressed endpoints. Candidates from the retained
household are read once each within the leave observation budget; another
household, an explicit address change, or unreadable self-evidence cannot
authorize cleanup. Only this confirmed-leave cleanup has that exception. The
native clear is still sent once, and a failed clear never starts another proof
or write attempt. The budget limits starting and accepting fallback observations;
an in-flight SOAP read remains bounded by the transport's own timeout.

Starting, extending or cancelling sends each native timer change once. A lost
or malformed reply can be confirmed through bounded reads only when the native
timer generation advances by exactly one, the requested countdown or cancellation
is observed, and the original physical group, address and known boot identity
still agree. An intervening replacement, even with the same countdown, cannot
supply that proof. Missing or malformed duration data never means cancellation.
Unavailable optional timer-generation evidence does not prevent an ordinary
acknowledged timer change once endpoint identity has been verified.

A definite native refusal leaves an existing timer and fade intact, so an
explicit retry can still cancel or extend it. An unconfirmed change returns an
uncertain result and retires the old local timer and fade; it cannot later pause
at a deadline the speaker may already have replaced. A running fade restores
only its still-owned levels, and no second native timer write is sent. Read the
speaker's current timer before choosing another action. An expired old dispatch
cannot remove a subsequently accepted extension.

Timer edits keep the existing saved-snapshot publication and retry behavior.
If publication fails and the process exits before retry succeeds, an older saved
timer can remain; restart still requires its matching native backstop before
adoption. This change does not make uncertain timer retirement durable across
that combined storage failure and process exit.

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

`cargo test -p fsonos-core --test sleep_ownership_sim` includes S1/S2 timer
mutation cases for definite refusals, lost and malformed replies, unavailable
read-back, changed generations, regrouping, reboot, running-fade retirement and
stale dispatch expiry. `cargo test -p fsonos-proto --lib control::tests` checks
strict timer evidence and overflow-safe duration parsing. The new cases have
passed the pinned formatter; their Rust execution remains pending.

`cargo test -p fsonos-core --test sleep_endpoint_sim` covers S1/S2 stale bindings,
actual address reuse, an unreachable old address, acknowledged changes without
native generation evidence, and announcement error cleanup at a changed endpoint.
Execution of these endpoint regressions remains pending.

`cargo test -p fsonos-core --test sleep_leave_cleanup_sim` covers S1/S2 stale
self-membership after a confirmed leave, an unavailable first witness, moved or
reused target and witness addresses, foreign-household exclusion, strict
self-read errors, and a lost native-clear reply without replay. Execution of
these regressions remains pending.
