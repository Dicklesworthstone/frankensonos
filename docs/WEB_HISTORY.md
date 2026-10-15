# Action history in the web remote

Open **Action history** beneath Scenes. It reads the same persisted action log
as `fsonos log` and `GET /actions`: who asked, which surface carried the call,
what was requested, the policy decision and the result. Denials, volume clamps
and incomplete operations are not hidden behind a success badge.

Select the newest 20, 100 or 500 entries and optionally an exact client name.
The text search only searches that loaded window. An empty result is not proof
that the daemon has never recorded actions. Timestamps use the browser's local
time; the stored instant is unchanged. A failed refresh preserves the prior
observation with an explicit stale notice. Changing the client clears the old
client's rows rather than presenting them as new results.

An `undoable` action means its row has a before-state. It does not prove that
it is still eligible for undo, belongs to the current caller, or can be fully
restored. Links to undo attempts are derived only from this loaded window.
Read the undo attempt's result for partial restoration failures. Reading or
filtering history does not send a mutation.

Run the client regressions with `node --test scripts/test-remote-history.cjs`.
The socket fixture tests the JSON client contract, not real Sonos hardware.
