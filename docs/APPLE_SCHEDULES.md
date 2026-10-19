# Schedules in the native Mac remote

Choose **Schedules** in the Browse menu. The panel lists all definitions returned
by the selected daemon, not just the playback sidebar's household. It works with
no selected room and even when topology reads fail, so an offline speaker cannot
prevent pausing or removing its future schedule. Schedule IDs remain Int64 and
existing stored room references are displayed verbatim.

## Create and manage

The form supports a weekly selection of days at HH:MM, or one explicit-offset
RFC 3339 time with whole seconds. Weekly wall times are in the daemon's house
time zone. The client never converts them to the Mac's time zone or calculates
next runs. One-time values include Z or a numeric UTC offset; the daemon rejects
past times. The native editor does not offer relative-delay or free-form schedule
syntax, although the daemon's CLI/API still supports those forms.

Choose an exact room and household for a DJ start, pause (optionally a fade of
0–600 seconds), or room volume (0–100). No room is selected implicitly. **Use
selected room** is an explicit copy into the draft; later sidebar changes do not
retarget it. Playback grouping is resolved at run time; scheduled volume changes
target the named room. A DJ mood may be left blank to follow the house program.
The daemon validates named moods and applies the creator's current policy.

For a scene, select its saved name. The review includes the currently saved
whole-house definition, re-read before creation. The schedule stores the name,
not a frozen definition: later runs apply the scene as saved then. A failed scene
catalog read does not discard the schedule list or disable room-based creation.

Each stored row shows its identity, definition, target, creator, enabled state,
daemon-reported next run and last processed run. The last processed timestamp
can refer to a skipped run; it is not proof of successful playback. Pause,
resume and removal each require review and a fresh read of that exact ID and
its definition. Resume uses the existing daemon rule that missed runs are not
made up. Removal receipts describe the definition that was removed, even if the
returned old definition was enabled. A run already executing may still finish.

## One command gate, no client scheduler

`ScheduleCommand` uses overloads on the existing `DaemonClient` and
`CommandReviewSession`. Playback, scenes, sleep timers and schedule mutations
share the same actor, pending-review UUID and busy slot. There is no second
writer or offline queue. A review is consumed before its first execution await,
so a double confirmation cannot create duplicate schedules. Low-level `perform`
remains available for library callers; the screen always uses the review session.

Creation reads the exact target's topology or saved scene, not speaker playback.
Management reads only `/schedules`, not `/zones` or `/state`. Definitions are
compared by ID, time, action fields, creator and enabled state; naturally changing
next/last-processed times and summary prose are not revisions. This is inferred
identity, not a transactional reservation: concurrent edits between preflight
and POST remain the daemon's responsibility.

Unsent work is cancelled on suspension, explicit target-selection changes or
leaving the panel. A cancelled read retains its busy slot until it drains. A
separate main-actor screen result keeps the original command and returned stored
ID/target, rather than attributing a late reply to the current playback selection.
A lost or malformed acknowledgement stays uncertain. Refreshing the list can
show a newly stored definition but cannot acknowledge that lost write, replay
it, remove it automatically, or clear its uncertainty message.

The editor draft survives refresh, panel changes and foreground transitions
within one connection session. It is not persisted across disconnect/process
exit, and no request is queued for later. Explicit refresh, panel entry,
foreground entry and command completion refresh the schedule list; a quiet panel
is not polled. The observation timestamp makes its age visible. Store actions
still get a fresh preflight, regardless of the displayed list's age.

## Verification

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
Twenty-one production-client/actor tests and seventeen form/screen tests cover
all four creation actions, pause/resume/remove, exact request fields, unchanged
house-local times under a Tokyo client time zone, full-width IDs, malformed and
3,200 mutated date strings, unavailable speakers, changed scenes/definitions,
failed reads, cancellation, shared playback serialization, delayed outcomes and
uncertain writes without replay. Controlled transports simulate storage and
replies; they do not execute the scheduled actions.

The existing Native Mac remote workflow runs the full native suite and builds
and packages the SwiftUI app on Apple SDKs. Linux checks alone do not establish
SwiftUI rendering, application launch, real tailnet access, daemon persistence,
actual schedule firing or physical-Sonos behavior. Those remain separate
integration and owner-LAN acceptance checks. No daemon API, Rust dependency or
scheduler implementation is changed by the native scheduling client.
