# Reviewed undo for agents

The MCP server exposes the same reviewed undo workflow as the HTTP API:

1. Call `preview_undo`. Its structured result is `{"action": <ActionDto>}` or
   `{"action": null}` when the caller has nothing eligible to undo.
2. Review the action's exact id, caller, intent and result. A preview is not a
   reservation or permission grant.
3. Call `undo_checked` with that positive `expected_id`. A changed, foreign or
   already-consumed target is refused; no other action is substituted.

The existing `recent_actions` permission authorizes the preview; `undo_last`
authorizes restoration. The new workflow never accepts an arbitrary caller,
`own_only=false`, a supplied before-state or permission to undo someone else's
work. Even a CLI identity uses only its own actions through this checked path.

The shared surface serializes selection/restoration with other undo calls and
excludes DJ queue maintenance during restoration. A second confirmation of a
consumed id cannot step backward into an earlier action. Other speaker commands
can still run concurrently, and restoring a before-state may overwrite later
changes. There is no redo.

## Keep the action's authority

With a configured daemon adapter, both calls require that daemon. If it becomes
unavailable, the checked workflow returns an error rather than switching to a
local action log whose numeric IDs could name unrelated actions. It never
falls back to the unbound `undo_last` endpoint. With no daemon adapter, both
calls use the backend's shared local surface instead.

A daemon that has its database replaced can reuse numeric IDs: this workflow
does not introduce a persistent database-generation identity. Reconnect and
review after replacing a store; do not carry old previews across that event.

## Report what actually came back

The reply includes the exact `undone` id, summary and structured `failures`.
Both `undo_checked` and `undo_last` set `isError` when any restoration failed,
preserving the complete receipt and listing each failure in the text an agent
reads. Complete restores and `nothing to undo` remain successful results.
A partial undo may already have consumed its action; inspect the receipt and
playback instead of automatically replaying it. A contradictory id is an
uncertain outcome, not a successful confirmation. A lost or unreadable reply
also requires inspection and a fresh preview. Cancelling a client call cannot
retract speaker I/O already sent.

Targeted regressions: `cargo test --locked -p fsonos-mcp --lib reviewed_undo`
and `cargo test --locked -p fsonos-cli --test mcp_reviewed_undo`.
The local cases use the real `Surface` and `MemStore`, with empty snapshots for
ownership and serialization checks; the daemon cases inject responses at the
adapter boundary. Full workspace and physical-speaker validation are separate.
