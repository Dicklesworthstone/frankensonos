# Announcement jobs and cancellation

Start speech, a generated chime, or an uploaded WAV and receive a job ID while
the daemon handles playback and restoration. Inspect that job from the CLI,
HTTP, MCP, phone remote, or dashboard, and cancel a queued or active clip.
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
| Wait for the final report | `POST /announce` | `announce` |

Start, inspection, and cancellation return HTTP 200 with the current job
observation. Admission means the daemon accepted responsibility for the job;
it does not mean the clip has played. A cancellation reply is also an
observation: continue reading the job until it has a terminal phase.

The job includes `id`, `kind`, resolved `rooms`, `created_at`, `finished_at`,
`phase`, `cancel_requested`, `can_cancel`, per-household `progress`, and a
human-readable `done` summary. `result` holds the final announcement report;
`error` describes a failure outside that report. Status and history contain
neither the spoken text nor the uploaded audio.

Read and cancel operations use the caller's current permissions. A caller sees
its own jobs; the local CLI identity and the configured house owner can inspect
and cancel all jobs. The `announce` policy class covers both synchronous and
asynchronous writes. Policies that allow exact tool names can grant
`announce_start`, `list_announcements`, `get_announcement`, and
`cancel_announcement` separately. An unknown, expired, or invisible job returns
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

The final `result.households` entries retain each household's `outcome`,
applied `levels`, `error`, and `not_restored` reasons. A cancelled clip is not
a successfully heard announcement, so its report can have `clean: false`
while the job phase is `cancelled`. A cancellation with incomplete restoration
ends as `failed` and preserves those reasons. Inspect the final receipt before
assuming the earlier playback and volume were restored everywhere.

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

## Capacity and lifetime

Background, synchronous, and scheduled delivery share the same admission
limits: two pending announcements per client and eight across the daemon.
These slots cover synthesis, waiting, playback, and restoration. Household
delivery remains serialized while independent households can proceed
concurrently. Room aliases are resolved at admission and their exact household
and primary-player identities are retained for delivery.

The daemon retains at most 64 active jobs and recent receipts, newest first.
It retires the oldest terminal entries first and never evicts active work to
make room. Jobs are process-local: restarting the daemon retires their IDs
and does not replay an interrupted clip. Use the existing
[announcement schedules](SCHEDULED_ANNOUNCEMENTS.md) when a future recipe must
persist across daemon restarts.
