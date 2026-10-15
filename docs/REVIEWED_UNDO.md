# Reviewed undo

`GET /undo/preview` reads the caller's newest eligible action as an `ActionDto`,
or JSON `null` when there is none. This uses the `recent_actions` permission,
reads only the store and does not reserve an action. The `client` in the row is
the daemon's caller identity: it may be shared by multiple browser tabs or
local applications, not a unique browser session.

`POST /undo/checked` accepts exactly `{"expected_id": <positive action id>}`.
It uses the existing `undo_last` permission and only the caller's own scope,
even for CLI identities. Unknown fields such as `own_only`, `client` or a
supplied before-state are refused. The legacy `POST /undo` is unchanged.

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
