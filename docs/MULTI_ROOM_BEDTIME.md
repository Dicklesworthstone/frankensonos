# Bedtime across rooms and households

Sleep timers and faded pauses accept the same targets as ordinary playback:
a room, a saved room alias, `all` / `everywhere`, or a household selector such
as `all@S1` or `all@S2`. Each existing playback group acts once, even when an
alias names several of its rooms. Groups keep their current membership and
their independent music; S1 and S2 groups can participate in the same request.

## CLI

```sh
fsonos sleep all 45m
fsonos sleep downstairs --extend 15m
fsonos sleep all@S2 --cancel
fsonos sleep downstairs
fsonos pause downstairs --fade 30s
fsonos schedule add "daily 22:30" pause downstairs --fade 2m
```

Replace `downstairs` with an alias in your `aliases.toml`. Setting a timer
replaces each selected group's current deadline. Extending adds the requested
duration to each group's own remaining time; a group without a timer reports
a failure while other selected groups can still extend. Cancelling an absent
timer is an unchanged success. Reading a target lists the running timers in
its groups; `fsonos sleep` without a target lists all running timers.

A daemon timer fades the group during its final two minutes, pauses playback,
and restores the rooms' original volumes. The speaker's own timer provides
the existing backstop. `--direct`, or a CLI/MCP command without a daemon, sets
the speaker's native timer and does not provide the daemon's timed fade.
Restart and interrupted-fade behavior are described in
[Sleep recovery](SLEEP_RECOVERY.md).

An immediate faded pause supports 1–60 seconds. A scheduled pause can fade for
up to 600 seconds. A sleep timer or extension supports up to 23 hours, and the
resulting extended timer must remain within that limit. Selected groups fade
with bounded concurrency. For a reviewed request or a multi-group target,
already paused or stopped groups remain unchanged.

For JSON callers, `--json` preserves the complete response. A partial timer
change exits unsuccessfully after printing every applied and failed result.
Inspect the affected groups before a new action: repeating a whole extension
would extend successful groups again.

## Phone and dashboard

On the phone remote, open **Control a room set or household** and choose
**Set sleep timers**, **Extend sleep timers**, **Cancel sleep timers**, or
**Fade out and pause groups**. On the dashboard, open a room's sleep timer
dialog and use **Bedtime across rooms or a saved room set**. Existing controls
for the selected playback group remain available.

The shared panel resolves the target into its actual physical groups, reads
each group's playback and current timer, and shows the proposed effect before
confirmation. It identifies existing timers that will be replaced, groups
that cannot extend because no timer runs, and groups already inactive before
a fade. Normal countdown progression does not invalidate a review.

Confirmation rechecks the complete target and current timer observations, then
sends one request with the reviewed audience revision. A changed alias or
physical group requires a fresh review. The phone reserves the affected groups'
existing playback and level lanes; the dashboard reserves its whole-house lane
while the request is pending. Each browser displays every returned
group result, and keeps lost, malformed or partial replies from triggering
automatic retries. Refresh the affected state before choosing another action.

## HTTP and MCP

| Operation | HTTP | MCP |
|---|---|---|
| Set timers | `POST /sleep` with `zone`, `duration` | `set_sleep_timer` with `zone`, `duration` |
| Extend timers | `POST /sleep` with `zone`, `duration`, `extend: true` | `set_sleep_timer` with the same fields |
| Cancel timers | `POST /sleep` with `zone`, `cancel: true` | `set_sleep_timer` with the same fields |
| Read selected timers | `GET /sleep?zone=downstairs` | `list_sleep_timers` with `zone` |
| Read all timers | `GET /sleep` | `list_sleep_timers` without `zone` |
| Fade and pause | `POST /pause/fade` with `zone`, `fade_secs` | `fade_and_pause` with `zone`, `seconds` |

Timer mutations and faded pauses accept an optional `expected_revision` from
`GET /rooms/resolve` / `resolve_rooms`. This identifies the reviewed target and
physical audience, not the timer's remaining duration. The server freezes the
resolved groups for dispatch and verifies each group's physical membership
before speaker writes. Editing an alias during dispatch does not add rooms to
the admitted request.

One resolved group retains the existing `SleepTimerDto` response and ordinary
error behavior. A target resolving to multiple groups returns a `SleepSetDto`:

```json
{
  "done": "sleep timer change in 45m: 1 group(s) confirmed; incomplete for 1 group(s)",
  "complete": false,
  "changed": true,
  "applied": [
    {
      "household": "S1",
      "coordinator": "RINCON_EXAMPLE_KITCHEN",
      "timer": {
        "zone": "Kitchen",
        "ends": "2026-11-23T03:45:00Z",
        "remaining_secs": 2700,
        "fades": true,
        "fading": false,
        "done": "Kitchen's group pauses in 45m",
        "changed": true
      }
    }
  ],
  "failed": [
    {
      "household": "S2",
      "coordinator": "RINCON_EXAMPLE_DEN",
      "zone": "Den",
      "error": {
        "code": "NOT_READY",
        "detail": "The selected group is unavailable",
        "hint": "Read the affected group's state before another action.",
        "suggestions": [],
        "retryable": false
      }
    }
  ]
}
```

The identities and timestamps above are illustrative. `applied` and `failed`
together account for every distinct selected group, including a request where
all groups fail. `changed` means at least one group confirmed a timer change;
a failed result may still describe an uncertain effect. HTTP returns the
batch receipt as a successful response envelope, so callers must inspect
`complete` and the per-group results. MCP retains the receipt and sets
`isError` for incomplete delivery, in both supported protocol eras. Faded
pauses use the existing `OutcomeDto.batch` per-group control receipt.

## Scheduled pauses and action history

Both browser routine forms offer **Pause rooms** with a **Pause rooms or saved
set** target. Choose a room, saved alias, `all` / `everywhere`, or a household,
and an optional fade of 0–600 seconds. Creating or editing the routine reviews
every affected physical group and rechecks the resolver revision before one
schedule write. Saving changes no current playback. The saved target remains
dynamic for future runs; an alias is resolved again for each occurrence.

A scheduled faded pause resolves and checks every selected group before its
durable occurrence claim. The admitted audience is retained through dispatch,
so an alias edit after the claim cannot expand that occurrence. Readiness
failures follow the scheduler's existing retry and grace behavior; delivery
failure after a claim does not replay the occurrence.

One request creates one action-history boundary. Timer changes and faded
pauses are non-undoable, as with their existing single-group counterparts.
Partial or uncertain writes cannot authorize undoing an earlier unrelated
action. Group receipts retain the evidence needed for a deliberate follow-up.

## Regression coverage

The API's `bedtime_targets_sim` tests exercise mixed households, deduplication,
single-group compatibility, partial results, lost replies, stale revisions,
physical audience changes and volume restoration. CLI `e2e_schedules` tests
exercise direct and daemon commands plus both MCP protocol eras. MCP unit tests
cover forwarding, error flags, schema compatibility and no-replay behavior.
`scripts/test-room-set-bedtime.cjs` exercises both browser panels, receipt
validation, request coordination, review invalidation and localhost HTTP
recovery. Rust runtime results must come from a successful Cargo/CI run; source
formatting and passing browser tests alone do not establish them.
