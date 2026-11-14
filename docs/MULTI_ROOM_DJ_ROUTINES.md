# Recurring DJ routines across rooms

A scheduled DJ start can target a multi-room alias, `all`, `everywhere`,
`all@S1` or `all@S2`. Each existing group starts its own feed once, even
when the target names several members of that group. A routine leaves
group membership intact and shares the immediate DJ batch control path.

## Create a routine

Keep `fsonos serve` running to execute schedules and maintain their DJ queues.
Use the same schedule command with a set of rooms:

```sh
fsonos schedule add "weekdays 07:30" dj start all --mood bright
fsonos schedule add "daily 18:00" dj start all@S2 --genre jazz --for 2h
fsonos schedule add "weekends 09:00" dj start upstairs --automatic
```

Here `upstairs` is a saved room alias. Define aliases with the existing
[room configuration](ROOM_CONFIGURATION.md) commands or editor. The
[multi-room DJ guide](MULTI_ROOM_DJ.md) describes immediate start, steer,
skip and stop controls.

The API uses the existing schedule shape. For example, send this body to
`POST /schedules`:

```json
{
  "when": "weekdays 07:30",
  "action": "dj_start",
  "zone": "upstairs",
  "direction": {
    "constraints": {
      "include_genres": ["jazz"]
    },
    "for_secs": 7200
  }
}
```

The `add_schedule` MCP tool accepts the same set in its existing `zone`
argument. Revision-bound schedule editing also accepts these targets and
keeps the schedule's identity, original creator and paused state.

## Phone and dashboard

Open **Scheduled routines** in the phone remote or **Routines** in the
dashboard and create a DJ routine. The **DJ rooms or saved set** field offers
individual rooms, saved aliases, the whole house and household-qualified sets.
Choose its time and optional mood or full direction, then review the routine.

The review resolves only the selected expression and lists each independent
group, including members affected through grouping but absent from the alias.
Saving reads that target again. A changed alias, physical group or mood requires
a new review. One schedule definition is saved; no DJ or speaker command runs
during creation. Each future occurrence resolves the authored expression again.

Edits retain the original creator. For an existing `here` routine, the browser
can only preview its current caller's default through the room resolver. The
review labels that preview separately and names the saved creator whose default
the scheduled run actually uses; those audiences may already differ. Saving
keeps `here`, the original creator, the reviewed routine revision and its
paused or enabled state. See [routine editing](ROUTINE_EDITING.md) for changed
revisions and recovery after an unacknowledged save.

## What the routine retains

The stored action keeps the authored target expression. It does not freeze
the coordinators discovered when the routine was created. Each occurrence
resolves the expression using its creator's current room defaults and
aliases, then deduplicates the existing groups.

`all` selects the groups currently discovered across households; a
household qualifier restricts that selection. An alias can name an explicit
set that must all be present before its occurrence starts. `here` follows
the original creator's configured default. Changing an alias or default
therefore changes which rooms a future occurrence selects.

Every selected group receives the same explicit mood or direction. A
direction's duration starts again on each occurrence. An empty direction
(`--automatic` in the CLI) clears previous session steering in each group.
A start with no mood or direction preserves each group's current steering.
Legacy mood-only routines retain their existing lifetime: until the DJ's
time-of-day program next changes, when known.

## Readiness and failures

Before claiming an occurrence, the scheduler checks its creator's current
DJ permissions, requires every explicitly named alias member, and prepares
every selected group's DJ start. A multi-group preparation checks the
renderer, music library and exact prospective direction, together with
volume reads required by the creator's caps. No group starts, receives new
steering or has its volume lowered while another group fails readiness.

Read-only readiness failures leave the occurrence due. The existing
30-second retry interval bounds repeated probes; an occurrence more than
ten minutes late expires without a delayed start. The admitted coordinators
and physical groups are retained across the occurrence claim: a later alias
edit or a newly discovered household cannot expand that run. Execution
rechecks the creator's current policy and group membership at mutation
boundaries, even if another client edited the routine.

Once claimed, an occurrence is consumed. A later speaker failure may leave
some groups completed or changed without acknowledgement. The scheduler
does not replay that batch. Its warning includes successful groups and
failed-group details, and the single action-history receipt retains the
same per-group results. An incomplete batch establishes an undo barrier
because restoring the entire batch cannot be proved safe.

Inspect the affected groups and action history before issuing another DJ
command. Future recurring occurrences retain their normal schedule.

## Regression coverage

The scheduled-DJ simulator tests use S1 and S2 households with grouped
members and independent rooms. They cover set and household-qualified
selection, coordinator deduplication, exact direction, all-group readiness,
audience changes during preparation and after claiming, alias edits, newly
discovered households, creator permissions and caps, one partial-result
receipt with complete scheduler failure detail, and no replay after a
consumed occurrence or scheduler restart.
