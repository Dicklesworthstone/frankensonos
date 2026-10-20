# Queue shuffle and repeat

HTTP, MCP, the CLI and the web remote can read and set the current group's
queue play mode without starting the queue, replaying the track, seeking, or
changing volumes. A room name or alias is resolved through fresh topology to
its group's coordinator. A paused queue stays paused. These controls do not
add, remove or reorder the stored queue entries.

## Read before changing

Read `GET /zones/{room}/play-mode`, call MCP `get_play_mode` with `zone`, run
`fsonos play-mode ROOM`, or open the web remote's **Shuffle & repeat** panel.
The response includes the coordinator's room (`zone`), `mode`, `queue_active`,
and `dj_active`. `queue_active` means the group's own queue is selected,
including while paused or stopped; it is not a claim that sound is playing.
An unavailable or unfamiliar setting is an error, never an assumed NORMAL.

| Mode | Order | Repetition |
|---|---|---|
| `NORMAL` | Queue order | Off |
| `REPEAT_ALL` | Queue order | Whole queue |
| `REPEAT_ONE` | Queue order | Current track |
| `SHUFFLE` | Shuffled | Whole queue |
| `SHUFFLE_NOREPEAT` | Shuffled | Off |
| `SHUFFLE_REPEAT_ONE` | Shuffled | Current track |

Send a complete mode, not separate toggle commands. In particular, `SHUFFLE`
means shuffle **and** repeat; `SHUFFLE_NOREPEAT` is shuffle without repetition.

## Change the mode

Send JSON with `Content-Type: application/json` to `POST /play-mode`, call
MCP `set_play_mode` with the same fields, or run
`fsonos play-mode Room SHUFFLE_NOREPEAT --expected-mode NORMAL`:

```json
{
  "zone": "Room",
  "mode": "SHUFFLE_NOREPEAT",
  "expected_mode": "NORMAL"
}
```

Use the real room and previously observed mode. `expected_mode` is optional;
when supplied, a different current setting is rejected before any write.
Omitting it does not suppress the later internal source/group/mode rechecks.
Only the six uppercase names above are accepted. An already-matching mode
returns `changed: false` and issues no native write or undo capture.

Writes require the group's own queue to be selected. For a radio, announcement,
line-in, or other source the call refuses rather than replacing that source.
Select an existing entry with `play_queue` first only when that is intended.
The running daemon rejects non-NORMAL modes while its DJ feeds the group:
whole works depend on sequential playback. Stop that DJ before changing order.
The CLI and MCP prefer the running daemon so they see that ownership and share
the log; the web remote always talks to the daemon.

The result includes `done`, the observed `state`, `changed`, and `recovered`.
`recovered: true` means SetPlayMode's reply failed but a subsequent read on the
same selected queue/group observed the requested mode. It is not proof that
this request, rather than another controller, caused the value.

There is no automatic write retry. Failed reads, conflicting source/group
changes, and unconfirmed settings after the write produce a nonretryable
uncertain error. Read again and make a new decision. A failed or malformed
daemon response never authorizes MCP to send a second local write.

## Policy, undo and boundaries

The policy actions are `get_play_mode` (read-only) and `set_play_mode` (write).
Unknown callers may read but cannot set. Existing per-client allowlists and
HTTP Host, Origin and JSON guards apply. No volume or playback-start operation
is part of a mode change.

When an action log and a successful capture are available, a mutation retains
the ordinary **full playback snapshot** for explicit undo. Undo can therefore
restore the earlier track position, transport state and levels as well as the
mode; it is not a mode-only reversal. Capture or log failures can leave no
undo state. No-ops and rejected preconditions do not create undo targets.

These are bounded read-before-write safeguards, not an atomic transaction or
lock shared with other controllers. A DJ can start or an owner can change the
source after the final preflight. Native modes and continuity on physical
S1/S2 firmware still require live acceptance; simulator coverage is not that
evidence.

The same controls are exposed by `fsonos play-mode`
([`CLI_PLAY_MODE.md`](CLI_PLAY_MODE.md)) and the web remote's
**Shuffle & repeat** panel; every surface uses the semantics above.
