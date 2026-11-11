# Reviewed undo

`GET /undo/preview` reads the caller's newest eligible action as an `ActionDto`,
or JSON `null` when there is none. This uses the `recent_actions` permission,
reads only the store and does not reserve an action. The `client` in the row is
the daemon's caller identity: it may be shared by multiple browser tabs or
local applications, not a unique browser session.

`POST /undo/checked` accepts exactly `{"expected_id": <positive action id>}`.
It uses the existing `undo_last` permission and only the caller's own scope,
even for CLI identities. Unknown fields such as `own_only`, `client` or a
supplied before-state are refused. The legacy `POST /undo` is separate: it
takes `own_only`, and `force` (the owner's CLI alone; anyone else is
`POLICY_DENIED`) to put back rooms someone changed since.

For a full-zone action that retained the rooms' state after the action, normal
undo checks that state again before restoring anything. If an addressable
room's source, volume or mute cannot be read, the entire undo is refused;
an unreadable room is not treated as unchanged. No target receives a restore
write and the action remains available to retry after the read recovers. A room
absent from the current inventory still reports a skipped restore while the
surviving zones can be restored. The owner's CLI can explicitly force an undo,
but current volume caps and per-step policy checks still apply.

The daemon serializes selection and restoration with all other undo calls.
It selects once, checks the id before restoration, and keeps the selected row;
a newer action cannot silently replace it. A missing, already consumed,
foreign or superseded target returns non-retryable `INVALID_ARGUMENT` (422)
with a hint to preview again. No restoration occurs on that mismatch. A
successful repeated confirmation cannot step backwards into an older action.

This is not whole-house isolation. Other speaker commands may run during the
restore, and restoring a recorded before-state can overwrite later changes.
The store remains locked per operation, not across speaker I/O; the existing
DJ restore exclusion and volume-policy checks still apply. A lost response or
log-write failure can leave an uncertain outcome; do not automatically retry.

Success uses `UndoDto`, with the exact `undone` id, `intent`, human summary and
`failures`. Partial zone restores now include skipped aspects in `failures`,
in addition to whole-zone, steering, tone and queue failures. Always show the
summary and failures rather than presenting every HTTP 200 as a full restore.

Regression coverage is in core `actions::checked_tests`, API
`surface::shared_store::reviewed_undo::tests`, the HTTP request tests and the
`log` DTO conversion tests. The empty-snapshot fixtures do not test physical
Sonos speakers; full workspace compilation and simulator tests remain required.

The protective-read regressions in
`crates/fsonos-core/tests/actions_sim/read_failures.rs` cover both simulated
generations, partial observations, retry, forced policy vetting and missing
inventory rooms. These new simulator cases have not run: the workspace build
was killed with exit 137 while compiling the existing `asupersync` dependency,
before it reached project checks. Formatting and source review do not substitute
for running these cases.
