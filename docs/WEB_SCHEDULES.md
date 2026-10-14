# Scheduled routines from the web remote

Open **Scheduled routines** on the daemon's web remote. Refresh to read the
saved schedules, their creators, and their next and last occurrences. A last
occurrence can have been skipped; it is not a claim that playback succeeded.

Choose **New routine**, then choose an action and an explicit destination:

- **Start the DJ:** a group and an optional mood; leaving the mood automatic
  uses the daemon's time-of-day program.
- **Apply a scene:** an existing scene. The review includes the scene's group,
  source, volume and mute definition.
- **Set room volume:** one physical room, including a member of a larger group,
  and a level from 0 to 100, still subject to house safety limits.
- **Pause a group:** optionally fade first, over 0 to 600 seconds. Zero pauses
  immediately when the routine runs, not when it is saved.

Choose daily, weekdays, weekends or particular weekdays and a house time.
Recurring wall times use the **daemon host's local time zone**, not the phone's
zone. The daemon handles daylight-saving transitions; the remote does not
implement another scheduler. Alternatively, run once after a delay (`45m`,
`1h30m`, `2d`, up to 365 days in this form), or at an exact RFC 3339 time with
seconds and an explicit offset (`Z` or `+/-HH:MM`). Relative delays begin when
the daemon accepts the request. An exact time already past is refused by the
daemon. After saving, the daemon's next-run timestamp is shown with both its
reported offset and the browser's local rendering.

**Review new routine** sends no write. **Create routine** creates an enabled,
persisted schedule through `POST /schedules`. It runs while the daemon is
running, with the creating client's permissions and house policy, even after
the browser closes. Runs missed by more than ten minutes are skipped.

## Managing and reviewing changes

Pause, resume and removal each require review. Pausing a routine does not pause
music or cancel an already-running occurrence. Resuming does not make up missed
runs. Removal deletes the saved routine, not its scene or music. Speaker undo
does not restore schedules.

The remote re-reads a reviewed schedule before modifying it. Creation rechecks
the selected group, scene and mood, and checks for an equivalent fixed-time
routine before sending. Creation and management share a single in-flight write
limit in the panel. Editing, refreshing or closing invalidates unsent reviews;
a stale confirmation button cannot approve a newer review.

These are preflight checks, **not an atomic cross-client lock**. Another client
can change a definition between the read and write. Schedules refer to rooms,
scenes and moods by name: future regrouping, scene edits or mood edits can
change what a later run does. A scene is not frozen at creation time.

If a write's reply is lost or contradictory, the remote does not repeat it. It
requires a fresh read. A creation may already have been saved: inspect the list
before creating another routine, particularly for relative delays, which have
no comparable fixed deadline until accepted by the daemon.

## Verification

`node --test scripts/test-remote-schedules.cjs` covers request construction,
weekday combinations, explicit-offset times, invalid input, stale reviews,
changed targets, concurrent calls and uncertain outcomes. Socket tests exercise
the shipped route map against a local fixture HTTP server, not a running Rust
daemon. The existing workspace and simulator tests cover the backend separately;
physical-speaker acceptance remains separate and opt-in.
