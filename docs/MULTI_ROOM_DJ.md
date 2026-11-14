# DJ control across rooms

Start, steer, skip or stop the DJ in every existing group selected by a room
alias, `all`, `everywhere`, `all@S1` or `all@S2`. Each distinct coordinator
acts once, even when an alias names several members of its group. Groups keep
their membership and run independent DJ feeds.

## Commands

```sh
fsonos dj start all --mood calm
fsonos dj start all@S2 --genre jazz --decade 60s --for 2h
fsonos dj steer upstairs --genre jazz --for 2h
fsonos dj steer everywhere --clear
fsonos dj skip all@S2
fsonos dj stop everywhere
```

Here `upstairs` is a saved multi-room alias. Use the existing
[room configuration](ROOM_CONFIGURATION.md) commands or editor to define it.
Ordinary room names, qualified room names and single-group aliases keep their
existing behavior.

A directed start gives every selected group the same explicit mood,
constraints and duration. Starting without direction preserves each group's
current steering or automatic program. Steering applies to later selections
and keeps already queued music; clearing restores the automatic program.
The shared engine retains its cross-group repeat avoidance.

These commands normally use the selected daemon, which keeps every feed
running. Direct mode retains its existing limitation: a start can play its
initial selections, but continuous queue maintenance requires the daemon.

## Phone and dashboard

Open **DJ across rooms** in the phone's DJ tools or the dashboard's DJ panel.
Choose a saved alias, a household or the whole house, or enter a qualified
room name. Review lists every affected group separately, including members
affected through grouping even when the alias does not name them.

Each group shows its current source, running or idle DJ, current whole work,
queued works and exact steering session. Start can preserve each group's
existing direction or apply one new mood, full filter set and duration to
all selected groups. The same panel saves direction, returns groups to their
automatic programs, skips whole works and stops the DJ. Existing single-group
direction and rehearsal tools remain available.

Confirming re-reads every affected group while holding the same command lanes
as other playback controls. A changed source, queue, session, mood definition
or audience retires the review. The browser sends one revision-bound daemon
request and never repeats it automatically. The phone also reserves affected
volume controls while a DJ start may apply caps; the dashboard shares its
existing house-wide command lane.

The result keeps every successful group and every `SKIPPED` note. An incomplete
or unacknowledged request offers **Read affected rooms**. This follows the
original player identities through their current grouping, reports missing
or unreadable rooms, and retains the original receipt. Closing and reopening
the panel preserves the recovery requirement. Observation does not prove which
earlier mutations completed; any new action requires a new review.

## HTTP and MCP

The existing routes accept the same expanded `zone` target; there is no
client-side loop of separate commands:

| HTTP route | MCP tool | Request |
| --- | --- | --- |
| `POST /dj/start` | `dj_start` | Target, optional mood and constraints |
| `POST /dj/steer` | `dj_steer` | Target and direction, or `clear: true` |
| `POST /dj/skip` | `dj_skip` | Target |
| `POST /dj/stop` | `dj_stop` | Target |

For example, send this body to `POST /dj/start`:

```json
{
  "zone": "all",
  "mood": "calm",
  "constraints": {
    "include_genres": ["jazz"]
  },
  "for_secs": 7200
}
```

HTTP durations use `for_secs`; MCP uses `for_minutes`, with constraint fields
at the tool argument level. The existing single-group response shape remains
unchanged.

### Bind an action to a reviewed audience

`GET /rooms/resolve?target=upstairs` returns the selected rooms, every affected
group and an opaque `revision`. Submit that revision with the same target to
`POST /dj/control`:

```json
{
  "zone": "upstairs",
  "expected_revision": "<revision returned by /rooms/resolve>",
  "action": "start",
  "mood": "calm",
  "constraints": { "include_genres": ["jazz"] },
  "for_secs": 7200
}
```

The action is `start`, `steer`, `skip` or `stop`. Direction fields follow the
existing requests; `clear: true` is valid only for steering. Skip and stop
reject direction fields rather than silently ignoring them. Each action uses
its existing policy permission and history name (`dj_start`, `dj_steer`,
`dj_skip` or `dj_stop`).

The revision binds the target's meaning, household identities and complete
physical groups, including bonded players, satellites and addresses. Addresses
are hashed and are not added to the preview. An alias expansion or changed
audience fails before readiness or any session/speaker mutation. After
admission the executor retains the reviewed group list and checks it again at
mutation boundaries, including when only one group was selected. A revision
is an observation, not a reservation; read the groups again after a refusal.

## Scheduled DJ across rooms

Scheduled DJ starts accept the same room sets, including a saved alias,
`all`, `everywhere`, `all@S1` and `all@S2`:

```sh
fsonos schedule add "weekdays 07:30" dj start all --mood bright
fsonos schedule add "daily 18:30" dj start downstairs --genre jazz --for 2h
fsonos schedule add "daily 21:00" dj start all@S2 --automatic
```

Creating or editing a routine validates every named alias member. At each
occurrence, the daemon resolves and deduplicates the current target, checks
every group's exact prospective direction, renderer, library and creator's
volume caps, and reconfirms physical membership before claiming the occurrence.
Unavailable prerequisites leave it due under the existing readiness backoff
and grace period.

The durable claim retains that admitted command list. An alias edit or newly
discovered group after the claim cannot enlarge the run. A changed physical
group is refused, even if the inventory has refreshed to the new topology.
The creator's current permissions still apply. A partially completed run has
one action receipt and is not repeated after a scheduler tick or restart.
Full directions and durations reapply on each run; a mood-only routine retains
its existing lifetime through the next automatic program change.

## Readiness, group changes and partial results

A multi-group start checks every selected group's renderer, library and exact
prospective direction, along with any volume reads required by the caller's
caps. A multi-group steer validates each prospective direction before saving
any of them. If a prerequisite fails, no group starts or receives new
steering, and the error reports both ready and failed groups. Clearing remains
available while the owner repairs invalid moods or preferences.

Skip and stop do not require a selectable music library. An idle or failing
group is reported while the other selected groups can complete.

Execution retains the original group identities and membership. Checks at
the engine's mutation boundary and before speaker writes reject a group that
changed while its command waited. A refusal blocks further writes for that
group. Volume caps and the original caller's permissions still apply.
If the audience changes after a new feed has queued music, that newly created
feed releases its ownership so later background refills cannot revive the
refused start. Its queued tracks remain available for an explicit observation.

A late speaker failure can leave other groups completed. The one action
receipt preserves every result:

- `done` identifies each successful group using a household-qualified room
  name, such as `Kitchen@S1`.
- `notes` contains a `SKIPPED` entry for each failed group, with its error code
  and detail. A failed group may already have changed before the failure.
- The CLI prints the complete receipt and exits nonzero when any group failed.
- MCP retains the structured receipt and marks an incomplete result with
  `isError: true`.
- HTTP preserves the existing `OutcomeDto` response. Clients must inspect
  `SKIPPED` notes even when the response is successful.
- If every execution fails, `OUTCOME_UNCERTAIN` retains all group failures in
  its detail.

Read affected groups before deciding on another command. Replaying a partial
batch can repeat an operation that already completed.

## Action history and undo

The batch has one action-log boundary. Its history retains failed-group
details as well as successful results. Successful multi-group steering
captures each group's previous session for the existing verified undo path.
An incomplete DJ batch creates an explicit undo barrier because a safe
whole-batch restoration cannot be inferred. Existing queue and source ownership
checks still govern whether other successful controls can be undone.

## Scope and verification

DJ status, why, feedback and rehearsal retain their existing target contracts.
Scene DJ entries continue to describe their individual scene groups. The
existing browser single-group editor remains available.

Regression coverage includes alias deduplication across S1/S2, exact direction
and caps, all-group readiness failure before writes, membership changes during
preparation and execution, partial skip/stop results, one action log, steering
undo, CLI exit status, MCP structured errors, and a CLI/HTTP/MCP daemon scenario
with independent feeds and cross-group repeat avoidance.

Additional coverage exercises reviewed-target alias expansion, physical player
and address changes, policy rejection before discovery, exact directed starts,
and one-group mutation fences. Scheduled tests cover mixed S1/S2 target sets,
all-group readiness, topology changes before and after the claim, alias and
discovery growth, creator caps and revocation, create/edit validation, and
partial completion without replay after a tick or restart.

The focused Node controller, DOM, shared-lane and served-bundle suites pass
250 tests, including 35 new cases. They exercise exact direction fields,
same-named S1/S2 groups, physical
and alias changes, lane waits, source/session changes, partial receipts,
lost replies, close/reopen, and observation after regrouping.
The full Chromium smoke script is included, but Chromium could not start in
this environment because its process socket was denied (`EPERM`); no browser
execution or visual result is claimed.

The changed Rust paths have been parsed and formatted with the pinned nightly
formatter. Execution of the Rust regressions remains pending while the shared
build host is under resource pressure; committed tests alone are not a passing result.
Focused entry points include:

```sh
cargo test -p fsonos-api --test dj_targets_sim
cargo test -p fsonos-api --lib surface::dj_control
cargo test -p fsonos-api --lib surface::players::targets
cargo test -p fsonos-api --lib surface::schedules::dispatch
cargo test -p fsonos-cli --bin fsonos dj::readiness
cargo test -p fsonos-cli --test e2e_dj_house
cargo test -p fsonos-mcp --lib partial_dj_results
node --test scripts/test-dj-targets.cjs scripts/test-dj-targets-panel.cjs
```
