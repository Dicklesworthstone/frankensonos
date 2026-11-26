# Announcement jobs, cancellation and recovery

Start speech, a generated chime, or an uploaded WAV and receive a job ID while
the daemon handles playback and restoration. Inspect that job from the CLI,
HTTP, MCP, phone remote, or dashboard, and cancel a queued or active clip.
If an interrupted recovery is still pending, you can explicitly abandon it
and keep the rooms' current playback.
The existing synchronous announcement commands and `POST /announce` still
wait for the final restoration report.

## CLI

Run `fsonos serve`, then start an announcement with `--background`:

```bash
fsonos say "Dinner is ready" --rooms Kitchen,Office --volume 25 --background
fsonos chime bell --rooms Kitchen --background
fsonos announce --file clip.wav --rooms Office --background

fsonos announcements
fsonos announcements list
fsonos announcements status <job-id>
fsonos announcements cancel <job-id>
fsonos announcements abandon <job-id> --confirm-abandon
fsonos --json announcements status <job-id>
```

Use the exact 32-character lowercase hexadecimal ID returned by the daemon.
`announcements` without a subcommand lists the same jobs as `announcements list`.
Jobs and `--background` require a running daemon; synchronous speech, chimes,
and WAV playback remain available in direct mode. Speech runs on the daemon
host, and WAV uploads send the client's bytes rather than a daemon filesystem
path. Existing room aliases, household-qualified names, voices, WAV validation,
and volume caps apply.

## HTTP and MCP

The asynchronous routes share the existing announcement request shape:

```json
{
  "text": "Dinner is ready",
  "rooms": ["Kitchen", "Office"],
  "volume": 25
}
```

Exactly one of `text`, `chime`, or `wav_base64` is required. `voice` is optional
for speech. The daemon validates the request and reserves capacity before
returning an admitted job; audio generation and playback continue afterwards.

| Action | HTTP | MCP tool in the full profile |
| --- | --- | --- |
| Start and return a job | `POST /announcements` | `announce_start` |
| List visible active jobs and recent receipts | `GET /announcements` | `list_announcements` |
| Inspect one job | `GET /announcements/{id}` | `get_announcement` with `id` |
| Request cancellation | `POST /announcements/{id}/cancel` with `{}` | `cancel_announcement` with `id` |
| Abandon pending recovery without speaker commands | `POST /announcements/{id}/abandon` with `{"confirm_abandon":true}` | `abandon_announcement_recovery` with `id` and `confirm_abandon: true` |
| Wait for the final report | `POST /announce` | `announce` |

Start, inspection, cancellation, and successful recovery abandonment return HTTP 200 with the current job
observation. Admission means the daemon accepted responsibility for the job;
it does not mean the clip has played. A cancellation reply is also an
observation: continue reading the job until it has a terminal phase.

The job includes `id`, `kind`, resolved `rooms`, `created_at`, `finished_at`,
`phase`, `cancel_requested`, `can_cancel`, `recovery`, `can_abandon_recovery`,
per-household `progress`, and a
human-readable `done` summary. `result` holds the final announcement report;
`error` describes a failure outside that report. Status and history contain
neither the spoken text nor the uploaded audio.

Read, cancel and recovery-abandonment operations use the caller's current
permissions. A caller sees its own jobs; the local CLI identity and the
configured house owner can inspect and act on all jobs. The `announce` policy class covers both synchronous and
asynchronous writes. Policies that allow exact tool names can grant
`announce_start`, `list_announcements`, `get_announcement`, and
`cancel_announcement` and `abandon_announcement_recovery` separately. An unknown, expired, or invisible job returns
`NO_MATCH`.

The initiating operation is checked again after synthesis, when each household
gets its turn, and after its snapshots before playback changes. A grant revoked
while a clip waits therefore prevents its dispatch. Existing sleep fades are
interrupted at preparation; a cancellation can resume an unchanged zone's fade
only after fresh observations confirm its original music and speaker state.

If a start reply is lost, inspect the job list before deciding to start again.
A network failure can occur after admission; retrying the creation request can
create another announcement. Cancellation uses the immutable job ID and is
idempotent: repeating it, including after completion, cannot replay a Stop.

## Progress and restoration

| Phase | Meaning |
| --- | --- |
| `queued` | Admitted, or waiting for another clip in the same household. |
| `preparing` | Generating the audio or preparing the speakers. |
| `playing` | At least one household is playing its clip. |
| `restoring` | Playback cleanup, original-state restoration, or completion recovery is still running. |
| `completed` | The job finished with a clean announcement report. |
| `cancelled` | Cancellation completed without a reported restoration failure. |
| `failed` | Preparation, playback, or restoration needs attention; inspect `error` and `result`. |

Households progress independently, so the top-level phase summarizes work that
may be at different stages. A job is terminal only after delivery and its
restoration callbacks return. `cancel_requested: true` alone is never proof
that the speakers have stopped or their earlier playback has returned.

Cancelling a clip waiting for its household turn releases it without sending
speaker commands. During local speech synthesis, cancellation interrupts the
engine at the next process poll (20 ms), terminates and reaps its process group
on Unix, and discards staged audio before it can become a playable clip. The
job releases its reserved turn and reports cancellation without speaker I/O.
Cancellation remains cooperative around in-flight network operations, which
may finish before the worker can observe it. Active cancellation stops the clip only while the daemon still
owns the announcement source and group, then uses the normal restoration path.
It does not overwrite a newer manual source, pause, regrouping, or other
ownership change.

Announcement preparation, group confirmation, and restoration require coherent
topology from a player identified at the address that answered. An empty,
inconsistent, or wrongly addressed response yields to another known household
player. If no player can establish that view, preparation leaves playback and its admission callbacks
untouched. The retained targets include each room's bonded speakers, including
missing stereo halves or home-theater members. Changed room membership cannot
inherit an admitted announcement, authorize its deadline Stop, or continue an
obsolete restoration. Room renames and ordinary coordinator changes retain their
existing behavior. The final receipt identifies anything left unrestored.

The final `result.households` entries retain each household's `outcome`,
applied `levels`, `error`, and `not_restored` reasons. A cancelled clip is not
a successfully heard announcement, so its report can have `clean: false`
while the job phase is `cancelled`. A cancellation with incomplete restoration
ends as `failed` and preserves those reasons. Inspect the final receipt before
assuming the earlier playback and volume were restored everywhere.

A speaker's `ERROR_OCCURRED` status ends delivery as a failure even when its
transport state still says playing or transitioning, or has already stopped.
The daemon confirms that the failed source is still its announcement, then
starts the existing ownership-checked restoration without waiting for the full
clip deadline. Status-only GENA events trigger that confirmation too. Newer
manual sources, pauses and grouping changes retain their usual protection.
The failed clip is not replayed. A background job ends as `failed` even when
all original music and levels were restored; inspect the per-household report
to distinguish delivery failure from incomplete restoration.

Regression coverage includes direct S1/S2 status combinations, accepted GENA
errors, an intervening source event and the public background-job receipt. Run
`cargo test -p fsonos-core --test announce_events_sim`,
`cargo test -p fsonos-core --lib announce::watch` and
`cargo test -p fsonos-api --lib renderer_errors_finish_background_jobs`.
These Rust cases remain unexecuted locally: the pinned `asupersync` dependency
compiler was killed before the workspace check reached project code.

## Phone remote and dashboard

Both announcement panels submit once, display the accepted job, and expose
its progress, cancellation, and final restoration details. The dashboard
releases its command lane after admission, so a long clip does not prevent
other controls from being used. Closing the announcement panel or navigating
away does not cancel the daemon's work.

The shared job observer reads only while its panel and page are visible.
It stops automatically after a bounded observation period or a failed read;
**Refresh jobs** resumes inspection. Cancellation is an explicit action on an
exact job. A lost or mismatched cancellation reply prompts a status refresh
without automatically repeating the command.

## Explicitly abandoning pending recovery

An unavailable or replaced player can keep an interrupted household pending.
Choose **Abandon recovery…** on that exact job, then confirm **Keep current
playback and abandon recovery**. The CLI and HTTP/MCP forms require the explicit
confirmation shown above. The `can_abandon_recovery` field is advisory: the
server checks ownership, permission and durable claim state again.

Abandonment leaves current playback, grouping, queue position, volume and mute
as found. It sends no speaker commands, including Stop. A clip still playing
can therefore continue until it ends or you control the room. The job receives
a terminal failed/incomplete receipt explaining that the previous state was
not restored. Its retired records no longer block new announcements in those
households; any other pending job still applies its own household restriction.

The daemon retires the entire exact unclaimed pending set for the original job
in one durable transaction. Previously completed household results remain
available. If any pending household was claimed by the recovery worker, its
proof changed, or another pending record appeared, the action refuses without
partially retiring the job. Cleanup already in progress must finish and publish
its receipt. Ordinary **Cancel** still requests restoration of an active clip.

Retirement is published before success is returned. A second request after
completion reads the retained receipt and cannot reopen recovery. If a response
is lost, refresh the job before deciding what to do; the browser consumes the
confirmation and never automatically repeats it. A capacity refusal leaves the
pending evidence unchanged. If journal publication or the response fails,
read the job again before assuming the retirement did or did not take effect.

The 98 Node announcement regressions passed, covering the explicit confirmation,
request identity, stale views, lost responses, and the shared phone/dashboard
job panel. Rust API, CLI/MCP and durable-journal cases cover caller/owner policy,
no speaker writes, complete-set retirement, claim races, restart persistence and
bounded receipt retention; these remain unexecuted because the current
environment has no Rust toolchain.

## Capacity and lifetime

Background, synchronous, and scheduled delivery share the same admission
limits: two pending announcements per client and eight across the daemon.
These slots cover synthesis, waiting, playback, and restoration. Household
delivery remains serialized while independent households can proceed
concurrently. Room aliases are resolved at admission and their exact household
and primary-player identities are retained for delivery.

The daemon retains at most 64 active jobs and recent receipts, newest first.
It retires the oldest terminal entries first and never evicts active work to
make room. Daemon announcements that reach their first durable checkpoint retain
per-household recovery evidence and recent receipts across restarts, under the
original job ID and caller identity. Queued work and unfinished synthesis that
have not reached that checkpoint remain process-local. Restart never replays an
interrupted clip. Use the existing
[announcement schedules](SCHEDULED_ANNOUNCEMENTS.md) when a future recipe must
persist across daemon restarts.

During an orderly stop, the daemon closes announcement admission and cancels
every accepted job before draining its workers. Waiting clips never start,
speech generation stops, and an active clip uses the same ownership-checked
restoration as explicit cancellation. This includes synchronous requests,
background jobs and scheduled announcements. A request still resolving its
rooms, or holding an earlier scheduler reservation, cannot start after that
shutdown boundary. Completed receipts stay unchanged.

The shutdown drain waits up to 60 seconds for this recovery, regardless of
the clip's original duration, and logs any work left in flight at the deadline.
A forced termination, host failure or kill after the service manager's timeout
cannot run the ordinary drain. The daemon's private `announcement-recovery.json`
records original snapshots, exact physical players and boot identities, caller
policy context, and attempted speaker changes before dispatch. It contains no
speech text or uploaded audio. The file is atomically replaced, synced, and
bounded to 64 household records and 8 MiB; unfinished records are never evicted
to admit another announcement. An unreadable or invalid journal is preserved
and refuses new announcement writes.

On restart, a supervised worker inspects each interrupted household. A clip
whose source selection or Play reached the speaker before the worker died can
be recovered even if the post-start ownership marker was never saved. This
requires the complete durable pre-dispatch record: original snapshots and
levels, attempted volume/mute changes, planned temporary grouping, prior source,
exact clip URL and known boot identities. The worker then proves the exact
clip, temporary group, endpoints and boot identities against fresh observations
before claiming cleanup. An already established clip uses the same proof. Newer playback, a paused
clip, regrouping, changed devices and listener level changes are preserved.
Current quiet-hour and caller caps still limit restored volume. Missing or
unreadable speakers remain pending, and automatic recovery expires after
24 hours. An affected household cannot start another announcement while that
inspection is pending; other households and ordinary playback controls remain
available.

Cleanup takes a durable claim before its first speaker write. If that cleanup
is interrupted again, the next process reports its uncertain result and does
not replay it. Earlier or incomplete preparation still remains an explicit incomplete result
for inspection. A fully prepared start is never enough on its own: the live
ownership proof must also succeed, and recovery never resends the clip. Recovery receipts
appear through the existing CLI, HTTP, MCP and browser job views. A crash leaves
delivery failed even when its owned music and groups were restored; it cannot
be reported as a successfully delivered clip. Recovery jobs have no original
playback worker to cancel; use explicit abandonment only for unclaimed pending
recovery when you intend to keep the current state.

A fade that the abandoned announcement could have interrupted is retired only
when its original coordinator and deadline still match. Recovery creates no new
sleep timer and preserves a replacement timer armed later. The speaker's own
sleep backstop is left intact. Direct CLI delivery without a daemon journal
retains the existing process-local cleanup behavior.
