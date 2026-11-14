# Edit an existing routine

FrankenSonos can change a saved routine's time, destination and action in place.
The routine keeps its id, creator, creation time and enabled/paused state. An
edit changes future occurrences without running an action on the speakers.
This supports all existing actions: DJ starts and directions, pauses and fades,
room volumes, saved scenes, speech and chimes.

## Phone and dashboard

Open schedules in the phone remote or **Routines** in the dashboard, refresh the
stored list, and choose **Edit routine** or **Edit** on the routine to change.
The editor fills in its current time and complete action. Both views support
all five action types and let you change the type while retaining the same
routine id. The dashboard's **New routine** form also supports all five types.

Changing only the time retains the saved action's details, including DJ
direction lifetimes in seconds, announcement voice and room scope, policy
default announcement volume, and existing room aliases. The review shows the
previous definition and proposed definition, the creator and paused state that
will be retained, and the effect on future runs. Choose **Save routine changes**
to send the reviewed revision once. Nothing plays immediately.

The editor refreshes the stored routine and its targets before sending. A
concurrent edit, toggle or occurrence invalidates the review. A draft remains
available after closing the panel, an unsuccessful read, or an uncertain
response. After an uncertain response, use **Refresh schedules** on the phone
or **Refresh routines** on the dashboard to inspect that same routine id. This
is a current observation, not proof of how the earlier request completed;
open a new edit and review it before submitting another change. The dashboard
uses its shared command lane to prevent overlapping writes and retains the
stored-routine recovery target even if the selected playback room changes.

Lists from older daemons remain readable. Their edit buttons are disabled when
the daemon has not supplied the required revision.

### DJ routines across rooms

For **Start the DJ**, both forms offer qualified rooms, saved aliases, the
whole house and household targets such as `all@S2`. Review shows every current
affected group and its complete room membership. A single alias can select
independent groups in both S1 and S2; each distinct coordinator starts once.
All existing direction filters and exact durations remain available.

The editor resolves the selected target only when reviewing it, then resolves
it again before the one create/update request. A changed alias, physical
audience or speaker address invalidates that review. Editing still requires
the current stored-routine revision and keeps its id, creator and paused state.
Saved offline targets remain explicit choices; the review reports when they
cannot currently resolve, and the daemon retains its final validation.

The routine saves the target expression. Future runs resolve it against the
house then, so alias edits or regrouping before a later occurrence may change
its audience. Each occurrence checks all selected groups before its durable
claim and retains that admitted audience through dispatch. See
[DJ across rooms](MULTI_ROOM_DJ.md#scheduled-dj-across-rooms).

## CLI

Read the routine and its current revision first:

```sh
fsonos --json schedule list
fsonos schedule edit 7 --expected-revision '<revision from the list>' \
  'weekdays 08:00' dj start 'Kitchen@S1' --genre jazz --for 2h
```

`schedule edit` accepts the same complete action syntax as `schedule add`.
Put `--expected-revision` before the action words; subsequent options belong to
the scheduled action. Examples include `pause Bedroom --fade 30s`,
`volume Kitchen 20`, `scene dinner`, `say 'Time to leave' --rooms Kitchen,Office`,
and `chime bell --rooms Office`. Global options such as `--json`, `--direct`
and `--daemon` can precede `schedule`.

The replacement is a complete definition. Include the current DJ direction,
announcement options or other action fields when retaining them. Listing with
`--json` exposes the complete stored definition. Commands use the selected
daemon when available; direct mode edits the durable data-directory store that
the daemon reads. A daemon error or lost reply never causes a second attempt
against the local store.

## HTTP and MCP

Every current `GET /schedules` row includes an opaque `revision`. Read it again
after editing, pausing/resuming, or a claimed occurrence. Older daemons may
omit this field; their lists remain readable, but an edit needs a current
revision from a daemon that supports updates.

`POST /schedules/update` accepts:

```json
{
  "id": 7,
  "expected_revision": "<revision from GET /schedules>",
  "schedule": {
    "when": "weekdays 08:00",
    "action": "dj_start",
    "zone": "Kitchen@S1",
    "mood": "bright",
    "direction": {
      "constraints": {"include_genres": ["jazz"]},
      "for_secs": 7200
    }
  }
}
```

The result is the updated `ScheduleDto`, including its revision and next
occurrence. MCP **`update_schedule`**, in the Full profile, takes the same three
fields: `id`, `expected_revision`, and the complete `schedule` object. All
action fields retain the `add_schedule` contract. Unknown fields are rejected;
a caller cannot change `creator`, `enabled`, creation time or the recorded
occurrence through this request.

## Concurrency and run behavior

The revision covers the complete semantic definition, owner, enabled state,
creation identity and recorded occurrence. The store compares the entire
originally observed row in the same transaction as the replacement. A competing
edit, pause, resume, removal or claimed occurrence prevents a stale edit from
overwriting newer state. `CONFIG_CHANGED` (HTTP 409) means read the current list
and review the replacement again. A deleted id returns `UNKNOWN_SCHEDULE`.

An identical definition is a verified no-op. It still checks the revision and
performs the atomic comparison, without moving the run cutoff. This also
permits an unchanged, completed one-shot without rescheduling it.

A changed definition advances the occurrence cutoff to at least the daemon's
current time while preserving any later recorded occurrence. Elapsed runs are
not made up. A changed one-shot must choose a time after that cutoff; relative
times such as `in 45m` resolve against the selected daemon's clock. Paused
routines stay paused until explicitly resumed. `last_fired` is an occurrence
and cutoff marker, not proof that audio played; the action log records delivery
outcomes.

Run admission compares the whole prepared definition atomically with its
occurrence claim. If an edit wins first, a run prepared from the old definition
cannot claim it. If a run wins first, the editor's revision becomes stale. An
already claimed run can finish; editing does not cancel it or undo speaker
commands. Pause, resume and removal also compare the observed row before
changing it, preventing a concurrent replacement from being overwritten.

## Permissions and action history

Editing requires `update_schedule`, in `house-config`, and the existing
owner/administrator rule for changing saved configuration. Both the editor and
the retained original creator must be allowed the new action and target,
including DJ steering when present. Every actual run continues to use the
original creator's current permissions and volume caps.

Editing sends no playback, volume, grouping or queue command. Changed
definitions are logged as non-undoable configuration changes with an explicit
action-history barrier; undo cannot reach through an edit to restore an older
routine definition. A confirmed stored change remains successful if the
separate action log cannot be written.

An uncertain response is never retried automatically. Read the selected
daemon's schedules before another reviewed change; a current observation does
not prove exactly how a previous lost request completed.

## Verification

`fsonos-core/tests/schedule_edit_store.rs` covers memory and real SQLite stores,
including two connections racing edits against each other or a prepared run,
stale removal, exact identity/occurrence checks, no-op behavior and reopening
the store. API tests cover retained creator rights, conflicts and no speaker
writes. CLI and MCP tests cover required revisions, complete action forwarding
and no local replay. The CLI daemon scenario edits a paused one-shot, refuses a
stale edit, reopens the durable store, resumes it and observes its new scheduled
occurrence.

`scripts/test-schedule-editing.cjs` exercises both browser views, shared forms,
exact action preservation, all five updates through a loopback HTTP fixture,
conflicting observations, response validation and recovery without replay.
The Linux workflow runs this alongside the existing schedule regressions.

`scripts/test-scheduled-dj-targets.cjs` adds 20 cases for room-set authoring in
both views, physical revision changes, full affected-group display, retained
offline targets and once-only create/update behavior. The four focused routine
suites pass 153 tests; the combined DJ/source/routine integration selection
passes 403 tests on the final browser changes.

Runtime results are reported separately from source and formatting checks;
adding a regression does not establish that it has executed successfully.
