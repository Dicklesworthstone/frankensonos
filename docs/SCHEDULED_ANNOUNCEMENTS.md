# Scheduled speech and chimes

Save spoken reminders and generated chimes with the same daemon-owned schedules
used for DJ starts, scenes, volume changes, and pauses. The daemon keeps the
recipe across restarts and generates its audio when the occurrence runs.

## CLI

```bash
fsonos schedule add "weekdays 08:15" say "Time to leave" --rooms Kitchen,Office --volume 25
fsonos schedule add "daily 12:00" chime bell --rooms Office
fsonos schedule add "in 45m" say "Check the oven" --room Kitchen --voice "Voice Name"
```

Use the same time syntax as other schedules: a delay, a one-time RFC 3339
timestamp with its UTC offset, or a recurring house-local time such as
`weekdays 08:15`. The daemon owns the clock; a browser or CLI does not
need to remain open.

`say` accepts up to 1,000 Unicode characters and an optional backend voice.
Speech uses the daemon host's configured offline speech engine.
`chime` accepts `bell`, `beep`, or `rise`. Chimes are generated locally
and do not require a speech engine. Schedules do not accept WAV uploads.

Repeat `--rooms`, separate names with commas, or use its `--room` alias.
Targets can be room names, household-qualified names such as `Kitchen@S1`,
or saved aliases. Omitting rooms means every discovered room at execution
time. Specify rooms explicitly when a reminder belongs only in particular
rooms. Volume is optional, from 0 to 100, with a default of 35; the current
house policy applies its caps.

List, pause, resume, or remove these definitions with the existing
`fsonos schedule` commands. Creating a schedule validates its recipe and
room targets without synthesizing audio or playing it.

## HTTP and MCP

`POST /schedules` and the MCP `add_schedule` tool use the same nested recipe:

```json
{
  "when": "weekdays 08:15",
  "action": "announce",
  "announcement": {
    "text": "Time to leave",
    "voice": "Voice Name",
    "rooms": ["Kitchen", "Office"],
    "volume": 25
  }
}
```

For a chime, replace `text` with `"chime": "bell"` and omit `voice`.
Exactly one of text or chime is required. The rooms, volume, and voice
belong inside `announcement`; unrelated top-level action fields are rejected.
The response preserves that complete recipe alongside the schedule ID,
creator, enabled state, next occurrence, and last recorded occurrence.

## Web and native clients

Choose **Make an announcement** in the web schedule form, select speech or a
chime, and choose the destination rooms. The review shows the full message or
chime, voice, rooms, and volume. Saving rechecks the selected room membership.
Saved rows retain the complete recipe for display, duplicate detection, and
pause/resume/remove review.

The native Apple client can display and manage these saved schedules.
Announcement creation is available through the CLI, HTTP API, MCP, and web form.

## Execution and recovery

Each run uses the creator's current rights and the existing announcement
delivery engine, including per-room caps and restoration of the previous
playback, volumes, and interrupted sleep-fade ownership. Changes to policy
after saving are applied when the reminder runs.

Before consuming an occurrence, the scheduler waits for its media listener,
target rooms, every named alias member, and an available announcement slot.
A busy announcement queue leaves the occurrence due while it remains inside
the scheduler's ten-minute grace period. Paused schedules stay quiet; an
occurrence missed beyond that grace period is recorded as skipped.

The daemon durably records an admitted occurrence before attempting delivery,
so another tick or restart does not repeat it. A delivery error after that
point is not automatically replayed because some rooms may already have heard
the announcement. Schedule responses describe stored definitions and recorded
occurrences; inspect the action log and daemon diagnostics for execution results.
