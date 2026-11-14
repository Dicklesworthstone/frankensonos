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
Scene and scheduled DJ actions keep their single-group plans. The browser DJ
controls continue to select one group; this addition expands CLI, HTTP and MCP
control.

Regression coverage includes alias deduplication across S1/S2, exact direction
and caps, all-group readiness failure before writes, membership changes during
preparation and execution, partial skip/stop results, one action log, steering
undo, CLI exit status, MCP structured errors, and a CLI/HTTP/MCP daemon scenario
with independent feeds and cross-group repeat avoidance.

The new Rust code and tests have been source-reviewed but have not been
compiled, executed or checked with rustfmt in this session. The build executor
was killed during dependency compilation and later became unavailable. Existing
repository CI is queued; committed tests alone are not a passing result.
Focused entry points include:

```sh
cargo test -p fsonos-api --test dj_targets_sim
cargo test -p fsonos-cli --bin fsonos dj::readiness
cargo test -p fsonos-cli --test e2e_dj_house
cargo test -p fsonos-mcp --lib partial_dj_results
```
