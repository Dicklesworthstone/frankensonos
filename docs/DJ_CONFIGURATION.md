# Reusable DJ moods and daily programs

The daemon can edit `moods.toml` through the phone remote, dashboard, CLI,
HTTP API, and MCP. A custom mood is a reusable set of selection rules. The
automatic program chooses a mood by weekday and time when a group's session
does not name one.

These settings apply to every DJ group managed by the daemon. Saving them
does not start music, replace a queue, skip a work, or clear temporary
steering. The new definitions take effect when the DJ makes its next picks.
Already queued music continues, and explicit session steering keeps its
existing precedence and expiry.

## Use the browser editor

Open **Moods & daily programs** on the phone remote or beside **Standing
taste** on the dashboard. No selected room or active DJ is required.

Select an existing mood or create a name such as `late-night-jazz`. Edit
artists, genres, composers, periods, decades, keywords, work length, energy,
long-work permission, or strict filtering. Names are entered one per line;
commas remain part of an artist's name. The complete definition also retains
song alternatives for classical periods and an optional absolute expiry.

The editor starts with a mood's full effective definition. Editing a built-in
such as `dinner` creates an owner override. **Restore default** removes that
override; **Remove custom mood** removes an owner-created name. A built-in
override replaces the entire definition, rather than merging individual
fields with a changing built-in.

Choose one of three program behaviors:

- **Use the built-in daily program:** follow the daemon's factory schedule.
- **Disable automatic mood changes:** leave the automatic mood unset; standing
  taste and temporary steering still apply.
- **Use my ordered daily program:** choose starting days, start/end times,
  and a mood for each row. Move rows to set precedence.

The first matching row wins. A Friday 21:00–02:00 row covers Friday evening
and Saturday before 02:00. Equal start/end times mean the whole selected day.
Times follow the daemon's local clock. The editor can preview the effective
mood for a selected day and time, including an unsaved restoration of defaults.
It displays the actual built-in definitions supplied by the daemon.

Review and save the whole draft. If another browser, agent, or local writer
has changed the configuration, the server rejects the stale save and the
editor retains the draft for comparison. A lost reply is never automatically
retried. **Check saved settings** reads back the file so the editor can establish
whether the submitted document was saved while retaining any newer draft edits.

## CLI

The configuration commands use the running daemon so it can check the live
DJ state and stored dependencies. They do not fall back to an independent
local writer when the daemon is unavailable.

```sh
fsonos dj configuration
fsonos --json dj configuration show > current.json
jq '.config' current.json > moods.json
```

Edit `moods.json`, then apply it with the revision from the same read:

```sh
fsonos dj configuration apply moods.json \
  --expected-revision "$(jq -r '.revision' current.json)"
```

The file contains the editable `config` object, not the entire response. The
explicit revision is required. A stale revision fails with `CONFIG_CHANGED`;
read again and merge the intended changes before submitting a new request.

For example, this document adds a strict jazz mood and replaces the automatic
program with two ordered windows:

```json
{
  "moods": {
    "late-night-jazz": {
      "include_genres": ["jazz"],
      "max_work_minutes": 18,
      "energy_bias": -1,
      "strict": true
    }
  },
  "programs": [
    {
      "days": ["fri", "sat"],
      "from": "21:00",
      "to": "02:00",
      "mood": "late-night-jazz"
    },
    {
      "days": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"],
      "from": "18:00",
      "to": "21:00",
      "mood": "dinner"
    }
  ]
}
```

With strict filtering, new picks wait if no work matches; filters do not
silently relax. Outside these two program windows the automatic mood is
unset. To retain the factory program while adding a custom mood, use
`"programs": null`. To disable the automatic program, use `"programs": []`.

## HTTP and MCP

| Operation | HTTP | MCP |
|---|---|---|
| Read editable and effective configuration | `GET /dj/configuration` | `dj_configuration` |
| Replace configuration at its revision | `POST /dj/configuration` | `dj_configure` |

The read response contains:

| Field | Meaning |
|---|---|
| `revision` | Opaque 64-character revision of the exact current file bytes. |
| `config.moods` | Owner-created moods and overrides only. |
| `config.programs` | `null` for defaults, `[]` for disabled, or an ordered replacement list. |
| `builtin_moods`, `builtin_programs` | Factory definitions, for previewing resets without hardcoded client data. |
| `moods`, `programs` | Effective definitions after applying the owner overrides. |

Both write surfaces accept the same request:

```json
{
  "expected_revision": "<revision returned by the read>",
  "config": {
    "moods": {},
    "programs": null
  }
}
```

That example resets all owner overrides and restores default programs, subject
to dependency checks. The successful response contains `done`, `changed`, and
`configuration` with the new complete read response. An identical document
returns `changed: false` and preserves the original file bytes and comments.

Reads use the ordinary read permission. Writes require `dj_configure`, included
in the `dj` policy class. Browser requests retain the daemon's same-origin and
JSON guards. Configuration changes are logged and do not create speaker undo
snapshots. The MCP tools are available in the full tool profile.

## Dependencies, persistence, and limits

Removing a custom name is rejected while standing preferences, an unexpired
stored DJ session, a saved scene, or a schedule still names it. Paused schedules
are included because they can be resumed. Update those references first.
Changing the rules under an existing name also checks compatibility with
retained work-length constraints: a mood's minimum cannot exceed a session or
routine's maximum after the normal override merge. Automatic directions are
checked against every distinct mood in the proposed automatic program. This is
conservative: it does not predict whether temporary constraints would expire
before a particular future window. Clear or adjust the conflicting direction
before saving that program.

A historical undo that would restore a removed mood or an incompatible named
or automatic direction is rejected before restoring speakers. Restore a
compatible definition or program before undoing that action.

Within the daemon, configuration changes serialize with scene and schedule
creation and checked undo. The engine retains its existing session and state
locking. It does not hold the database mutex during an engine operation.

Saves validate the proposed configuration and its dependencies before writing,
then compare the exact file revision under a stable sibling lock. They write
a unique temporary file, sync it, rename it into place, and sync the directory
on Unix. Cooperating writers cannot overwrite each other's stale edits. A
directory-sync failure after publication returns `OUTCOME_UNCERTAIN` and asks
the caller to read back; it does not replay the write.

Manual TOML editing remains supported, including existing `days = "weekdays"`,
`"weekends"`, and `"daily"` aliases. API reads expand these to explicit weekday
lists. A manual editor that ignores the sibling lock can still race the final
rename; revision checks detect edits that occur before the comparison.

Remote edits allow up to 128 custom moods/overrides, 256 program rows, and
256 KiB of configuration. Shared filter limits remain 32 entries per list,
128 characters per entry, work lengths of 1–600 minutes, and energy bias
from −2 through 2. Unknown request fields are rejected rather than silently
dropped. Reads preserve all existing constraint fields, including
`song_keywords` and `expires_at`; the browser refuses a definition it cannot
represent faithfully.
