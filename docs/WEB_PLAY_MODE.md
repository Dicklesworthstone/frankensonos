# Shuffle and repeat from the web remote

Open **Shuffle & repeat**, choose a household-qualified group, and read its
current setting. Choose a complete playback order and press **Apply playback
order**. Changing a choice alone sends no command. The panel is lazy: it does
not poll closed controls or need to stay open after the change.

The six choices match the shared API's six modes: in order with no repeat,
repeat the queue, repeat the current track, shuffle with no repeat, shuffle
and repeat the queue, or shuffle and repeat the current track. In particular,
`SHUFFLE` includes queue repeat; `SHUFFLE_NOREPEAT` does not.

The group must already have its own queue selected, playing or paused. A
radio or another source stays read-only. The panel never starts playback,
selects a source, seeks, changes volume, or rebuilds a queue to make a mode
change work. An active DJ permits only NORMAL order; stop it separately
before choosing shuffle or repeat. Unknown modes and failed reads are errors,
not an assumed NORMAL setting.

The panel calls GET /zones/{room}/play-mode and POST /play-mode through the
same-origin API and retains its identity, policy, logging and response rules.
It sends the exact observed setting as expected_mode. Before dispatch it
rechecks the group's members and the source/DJ/mode observation. A changed
view requires a new read. This is a bounded read-before-write check, not an
atomic lock against the speaker or another controller.

Only one change can be in flight in this panel. Closing during preflight
prevents dispatch; closing after dispatch does not cancel a speaker operation.
A missing, malformed, rejected or lost reply never triggers an automatic
second change. Read the current setting explicitly before deciding what to
do next. Confirmed server read-back recovery is displayed as the server
reports it. A no-op choice makes no request.

The existing API can log a full playback snapshot for explicit best-effort
undo. That is not mode-only undo: see PLAY_MODES.md before using it. The web
panel does not automatically restore playback on failure.

Regression coverage executes the shipped JavaScript controller, including
all thirty distinct mode transitions and six no-op choices. Browser component
fixtures use synthetic API responses; they do not establish real-daemon or
physical-speaker behavior. The Linux workflow also runs the controller suite.
