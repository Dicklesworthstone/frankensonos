# Queue playback order from the CLI

`fsonos play-mode ROOM` reads the group's current queue-order mode, whether
its queue is selected, and whether the daemon's DJ owns its order. With no
mode argument it is always a read: it never normalizes or starts playback.

```sh
fsonos play-mode 'Room@S2' --json
fsonos play-mode 'Room@S2' SHUFFLE_NOREPEAT --expected-mode NORMAL
```

The second command sets the whole shuffle/repeat combination. The optional
`--expected-mode` is the setting from a preceding read; a mismatch refuses
the change instead of overwriting another controller's setting. It requires
a desired mode. Without that option the command deliberately requests an
absolute setting, not a toggle. The shared backend still checks source and
group stability before dispatch and reads the mode back afterwards.

The accepted names come directly from the shared `PlayMode` catalog:

| Mode | Behavior |
| --- | --- |
| `NORMAL` | Queue order, no repeat |
| `REPEAT_ALL` | Queue order, repeat the queue |
| `REPEAT_ONE` | Repeat the current entry |
| `SHUFFLE_NOREPEAT` | Shuffle without repetition |
| `SHUFFLE` | Shuffle and repeat the queue |
| `SHUFFLE_REPEAT_ONE` | Shuffle with current-entry repeat |

Use the exact uppercase names shown by `--help`. Unknown settings are not
converted into NORMAL. The speaker must already have its own queue selected,
playing or paused. Radio and other sources cannot be made writable by
implicitly switching to the queue. A mode change does not start playback,
seek, alter the volume, or reconstruct the queue. A running daemon DJ permits
NORMAL order only: stop it explicitly before choosing shuffle or repeat.

All ordinary global options work, including `--json`, `--daemon`,
`--daemon-url`, and `--direct`. Automatic mode prefers a running daemon and
otherwise uses the same shared action layer directly. Errors after daemon
dispatch are returned without retrying locally. The direct process cannot
observe another process's active DJ session; use the daemon while its DJ is
running. Backend read-before-write checks are not an atomic lock on speakers.

Reads have no undo record. Successful mode writes use the existing action log
and playback-snapshot undo. This is not mode-only undo: it may also restore
source, position, transport, levels or grouping according to that snapshot.
See PLAY_MODES.md before using `fsonos undo`. An uncertain result never causes
automatic rollback or a second mode write; read the current setting again.

The parser tests cover all six names and all 36 wanted/expected pairs,
read-only defaults, path encoding, global options, and invalid combinations.
The CLI/simulator tests cover all modes on S1/S2, a paused queue, member-to-
coordinator routing, stale guards, non-queue rejection, and daemon logging.

```sh
cargo test -p fsonos-cli --bin fsonos play_mode_cmd
cargo test -p fsonos-cli --test e2e_play_mode
```

Simulator coverage is not physical-speaker qualification. A committed test is
not evidence of a passing run; inspect a recorded verifying run or run the
commands above.
