# Reusing DJ directions in scenes and routines

A DJ direction contains the same selection constraints as `dj start` and
`dj steer`: artists, genres, decades, keywords, composers, classical periods,
work lengths, energy, and their exclusions. Scenes and scheduled DJ starts
can retain this complete direction instead of only its named mood.

## Capture a session and recall it later

```bash
fsonos dj start Kitchen --genre jazz --artist 'Miles Davis' --for 2h
fsonos scene save jazz-evening
fsonos scene show jazz-evening
fsonos scene apply jazz-evening
```

Capturing a running DJ saves its explicitly added constraints, its session
mood, and the remaining duration at capture time. If 75 minutes remained,
recalling the scene grants a fresh 75 minutes. A direction without an expiry
stays without an expiry. An expired or absent override is captured as an
explicit return to the automatic program. Derived time-of-day or mood filters
are not copied into the saved constraints.

The saved direction is applied before starting the DJ, so its first new pick
uses those settings. Starting a feed that already runs preserves its queued
works, as an ordinary constrained `dj start` does. A scene is not a recording
of a DJ's future playlist or an instruction to replay already queued works.

## Schedule a complete direction

```bash
fsonos schedule add "weekdays 07:30" dj start Kitchen \
  --genre jazz --artist 'Earth, Wind & Fire' --artist 'Miles Davis' \
  --not-genre pop --max-minutes 20 --calmer --for 2h

fsonos schedule add "daily 18:00" dj start Kitchen --automatic
```

Every firing starts a fresh relative duration. The second example clears
temporary steering before starting the automatic program. `--automatic`
cannot be combined with a mood, constraints, or duration. Repeated name flags
preserve each name, including commas in artist names.

Existing bare starts and mood-only schedules retain their behavior: a bare
start preserves the current steering; a mood-only scheduled start lasts
until the next time-of-day program change. Add constraints or `--for` to save
an explicit reusable direction, or use `--automatic` to request a reset.

## Create directions in the web remote

In **Manage scenes → New custom scene**, choose the group and DJ source,
then open **DJ filters and duration**. The same editor is available under
**Scheduled routines → New routine → Start the DJ**. Choose an optional mood,
enter names one per line, and give a duration in minutes if needed. Commas
remain part of artist names; release decades use the full first year, such
as `1960`.

The review lists the complete direction before saving. Editing a filter or
duration retires that review. A saved reply must retain the exact selection
rules; a changed or incomplete reply is not reported as a verified save.
An empty new DJ direction explicitly returns to the automatic program when
the scene or routine runs. Saving either definition does not start music.

## HTTP and MCP

`POST /schedules` and the `add_schedule` MCP tool accept the same optional
`direction` object:

```json
{
  "when": "weekdays 07:30",
  "action": "dj_start",
  "zone": "Kitchen@S2",
  "direction": {
    "constraints": {
      "include_genres": ["jazz"],
      "include_artists": ["Miles Davis"],
      "max_work_minutes": 20,
      "energy_bias": -1
    },
    "for_secs": 7200
  }
}
```

A scene source accepts that object alongside `kind: "dj"` and optional
`mood`. For a scene authored with `scene save --from`, the corresponding TOML
is:

```toml
[[groups]]
coordinator = "Kitchen@S2"
members = []
playing = true

[groups.source]
kind = "dj"
mood = "calm"

[groups.source.direction]
for_secs = 7200

[groups.source.direction.constraints]
include_genres = ["jazz"]
exclude_keywords = ["vocal"]
max_work_minutes = 20
```

Omitting `direction`, or sending `null`, preserves the legacy behavior. An
explicit empty `direction: {}` without a mood clears the previous override.
The two meanings remain distinct in saved storage and client review screens.

`for_secs` is relative, bounded to 1–604800 seconds, and requires a mood or
at least one constraint. Capturing the final seconds of an active direction
therefore preserves its exact remaining lifetime. Unknown constraint fields,
invalid ranges, or a direction attached to another source/action are refused.

A directed start requires both `dj_start` and `dj_steer` permission. Scene
recall checks the caller and the scene's saver; a scheduled run checks the
stored creator's current policy. Malformed or changing stored steering aborts
capture instead of silently replacing it with an automatic direction.
