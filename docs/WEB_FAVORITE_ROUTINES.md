# Favorite routines in the web remote

Open the full web remote at `/`, choose **Routines**, and expand **Favorite routines**. Give the
routine a name, choose a current household-qualified group and a playable
favorite, then choose weekdays, every day, weekends, or a one-time delay of
1–10080 minutes. Review the favorite, exact group, timing and playback effects
before choosing **Save scene and create routine**.

This connects two existing capabilities rather than introducing a second
scheduler or player. It saves a dedicated scene, then creates an enabled
`apply_scene` schedule. Manage its future runs in **Routines** and its saved
scene in **Scenes**. The daemon executes it after the browser closes, provided
the daemon is running and the schedule's creator still has permission.

Daily and weekly times use the daemon host's local time zone, not the browser's.
The existing scheduler handles daylight-saving transitions and skips occurrences
more than its ten-minute default grace late. A delay starts when the daemon
accepts the schedule, not when the review opens. The returned schedule reports
its stored time and next occurrence. Setup sends no playback command itself;
the enabled schedule may run as soon as its own deadline arrives.

## What will change when it runs

The scene saves the selected group's coordinator and exact room membership,
qualified by household. Each execution restores that grouping: listed rooms
may leave other groups, and unlisted rooms joined later may be detached. The
review spells this out before either saved change is sent. An album or playlist
favorite replaces the group's queue; a track or stream replaces its playback
source. The scene contains no volume or mute settings and does not unmute a
room or raise its level to make an alarm audible.

A scene is not a frozen playlist or a queue backup. Its favorite is resolved by
the existing scene engine, which stores the resolved title and URI when saving.
Later scene edits, renamed or ambiguous favorites, account changes and altered
playlist contents can change or prevent a future run. The composer does not
claim an immutable favorite identity or whole-house transactional isolation.

## Setup and recovery

Before saving, the composer reads the current group and favorites again. A
changed membership, household, selected favorite or colliding scene name
invalidates the review without sending a write. Its dedicated scene name uses
128 random bits from browser cryptography; it never silently reuses an existing
user-named scene. This avoids ordinary collisions but is not a server-side
create-only transaction with another controller.

The only writes made by this composer are:

```text
PUT  /scenes/{generated-name}
POST /schedules   { when, action: "apply_scene", scene: generated-name }
```

The PUT contains a favorite scene definition and empty volume/mute maps. Its
response must match the reviewed group and favorite. A fresh scene observation
must then match the acknowledged definition, including its saved time and
resolved URI. Existing schedules referencing that scene prevent a second add.
The shared remote request adapter remains responsible for ordinary HTTP
failures and deadlines; neither write is automatically retried.

Submission intent is written to this tab's session journal before each request.
An unreadable, unavailable, oversized or non-persisting journal blocks further
setup rather than silently falling back to an untracked write. Recovery states
are explicit:

- **Scene saved; routine not submitted:** inspection found the reviewed scene,
  and this attempt has not submitted a schedule. A new explicit review can
  authorize the one missing schedule request.
- **Setup needs inspection:** a write may have completed or may still finish.
  **Inspect saved state** only reads. An absent schedule after a submitted POST
  never authorizes replay. A matching row is described as an observation, not
  proof that a lost request succeeded.
- **Routine saved or observed:** its ID, enabled/paused state, stored time and
  reported next run are shown. **New favorite routine** explicitly clears this
  completed attempt's journal; it does not remove a scene or schedule.

There is no automatic rollback. A saved scene can remain after a later failure;
check its references in Routines before deleting it through normal scene
management. Removing a routine does not remove its dedicated scene. Closing or
hiding the panel invalidates future submissions, but cannot cancel a request
already sent to the daemon. Dispatch checks the native details element's open
state, not only its asynchronously delivered toggle event. The composer mounts
as a native section in the Studio Routines workspace. Hidden or inert ancestors
also fence dispatch, and workspace navigation discards dormant reviews.

Session storage is per origin and tab. It survives a reload of the same tab,
not necessarily closing the tab or clearing browser data. It is not a shared
lock between tabs, callers or devices. Inspect existing scenes and schedules
before recreating an uncertain attempt in a new tab.

## Validation

```sh
node --test scripts/test-favorite-routines.cjs scripts/test-favorite-routines-panel.cjs
python scripts/test-favorite-routines-component.py --chromium /path/to/chromium
```

The Node suite has 35 passing tests for request contracts, immutable reviews,
changed membership/favorites, duplicate dispatch, lost and malformed replies,
scene changes, partial setup, journal failures, restart-style model recovery,
asset embedding and a real loopback HTTP fixture. Chromium component checks
have 12 passing cases covering actual controls, review/Escape focus, timing,
recovery/remount, closing during a pending PUT, hidden Studio workspaces, visibility, hostile metadata and
1440/768/390/320-pixel widths. Component transport and storage are in-memory
fixtures, not a real daemon or browser reload test.

The editing environment blocked browser loopback navigation. The Node HTTP
fixture did run; browser-to-daemon acceptance and real session-storage reloads
remain unverified. Rust embedding changes were checked against the exact parent
file and by static asset tests, but no Rust compiler was available. Workspace
compilation, physical S1/S2 playback and actual scheduled scene execution still
require their normal acceptance gates. The backend scheduler, scene executor,
policy and speaker protocols are unchanged.
