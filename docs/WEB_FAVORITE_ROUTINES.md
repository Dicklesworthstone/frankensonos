# Music routines in the web remote

Open the full web remote at `/`, choose **Routines**, and expand **Music routines**.
Give the routine a name, choose its music source and a current household-qualified
group, then choose weekdays, every day, weekends, or a one-time delay of
1–10080 minutes. Optional volume and unmute settings appear below the timing.
Choose **Review music routine**, check the source, exact rooms, timing and
playback effects, then choose **Save scene and create routine**.

The composer supports three sources:

- **Sonos favorite:** choose a playable track, stream, album or playlist from
  the selected household's favorites. Existing favorite routines still work.
- **Spotify track, album or playlist:** paste a canonical Spotify URI or its
  full `open.spotify.com` / `play.spotify.com` link. Locale, embed, tracking and
  legacy playlist forms are normalized; review shows the canonical source.
  Artist, episode and show links are not offered by the scene engine. Short
  links must be opened separately; the browser does not expand them or contact
  Spotify. Playback still needs the daemon and speakers' existing Spotify setup.
- **Radio stream URL:** paste a direct HTTP(S) audio stream, not the station's
  web page. The speaker must be able to reach and decode it. Embedded user-info,
  fragments, backslashes, whitespace and overlong input are rejected. Avoid
  private access keys: a source is stored in the scene and the tab's journal.

Spotify and radio routines do not require favorite discovery. Changing the
source mode changes only the form and, when needed, its read-only choices.
The supported URI subset is checked locally, then validated again by the daemon;
saving a scene is not a playback or reachability test.

This connects existing capabilities rather than introducing another scheduler
or player. It saves a dedicated scene, then creates an enabled `apply_scene`
schedule. Manage its future runs in **Routines** and its saved scene in **Scenes**.
The daemon executes it after the browser closes, provided the daemon is running
and the schedule's creator still has permission.

Daily and weekly times use the daemon host's local time zone, not the browser's.
The existing scheduler handles daylight-saving transitions and skips occurrences
more than its ten-minute default grace late. A delay starts when the daemon
accepts the schedule, not when the review opens. The returned schedule reports
its stored time and next occurrence. Setup sends no playback command itself;
the enabled schedule may run as soon as its own deadline arrives.

## What will change when it runs

The scene saves the selected group's coordinator and exact room membership,
qualified by household. Each execution restores that grouping: listed rooms
may leave other groups, and unlisted rooms joined later may be detached.
An album or playlist can replace the group's queue; a track or stream changes
its playback source. The review identifies these effects before either write.

**Volume is optional.** Blank leaves current levels unchanged, subject to the
house policy. A number from 0 to 100 requests that level for each saved room,
not an aggregate group volume. Zero deliberately requests silence. Each room's
policy cap and the existing scene execution/fade behavior still apply. The
composer does not add a separate volume command during setup.

**Unmute is separately opt-in.** Unchecked preserves mute settings. Checked
stores an explicit unmute for every saved room. Setting a nonzero volume alone
does not promise audible playback; a muted room may stay muted. Source switches
and cancelled reviews retain the chosen settings; **New music routine** resets
them to the non-mutating defaults.

A scene is not a frozen playlist or a queue backup. Favorites are resolved by
the scene engine, which stores the resolved title and URI when saving. Later
scene edits, renamed or ambiguous favorites, account changes, altered playlists
and unavailable streams can change or prevent a run. The composer does not
claim immutable content identity or whole-house transactional isolation.

## Setup and recovery

Before saving, the composer reads the current group again. Favorite routines
also reread the selected favorite. Changed membership, household, favorite or a
colliding scene name invalidates review before any write. The dedicated scene
name uses 128 random bits from browser cryptography; it never silently reuses
an existing user-named scene. This avoids ordinary collisions but is not a
server-side create-only transaction with another controller.

The only writes made by this composer remain:

```text
PUT  /scenes/{generated-name}
POST /schedules   { when, action: "apply_scene", scene: generated-name }
```

The PUT contains a `favorite` or `uri` scene definition, exact group membership,
and the reviewed volume/mute maps (empty by default). Its response must match
the reviewed source, rooms and settings. A fresh scene observation must then
match that acknowledgement, including saved time and resolved URI. Existing
schedules referencing the scene prevent another add. Neither write is retried
automatically; ordinary HTTP deadlines remain the shared client's responsibility.

Submission intent is written to this tab's session journal before each request.
Unreadable, unavailable, oversized or non-persisting storage blocks further
setup instead of silently falling back to untracked writes. New records use
version 2. The same session-storage key is retained so version-1 favorite
attempts can still be inspected and explicitly continued after an update.
Contradictory source, settings or scene data blocks recovery.

- **Scene saved; routine not submitted:** inspection found the reviewed scene,
  and this attempt has not submitted a schedule. A new explicit review can
  authorize the one missing schedule request.
- **Setup needs inspection:** a write may have completed or may still finish.
  **Inspect saved state** only reads. An absent schedule after a submitted POST
  never authorizes replay. A matching row is described as an observation, not
  proof that a lost request succeeded.
- **Routine saved or observed:** its ID, enabled/paused state, stored time and
  reported next run are shown. **New music routine** clears this completed
  attempt's journal without removing its scene or schedule.

There is no automatic rollback. A saved scene can remain after a later failure;
check its references before deleting it through normal scene management.
Removing a routine does not remove its dedicated scene. Closing or hiding the
panel invalidates later submissions but cannot cancel an already-sent request.
Dispatch checks native open state, document visibility and hidden/inert Studio
ancestors. Navigation discards dormant reviews; Escape cancels review and
returns keyboard focus without sending a write.

Session storage is per origin and tab. It survives a reload of the same tab,
not necessarily closing the tab or clearing browser data. It is not a shared
lock between tabs, callers or devices. Inspect existing scenes and schedules
before recreating an uncertain attempt in a new tab.

## Validation

```sh
node --test scripts/test-favorite-routines.cjs scripts/test-favorite-routines-panel.cjs scripts/test-music-routines.cjs
python scripts/test-favorite-routines-component.py --chromium /path/to/chromium
```

The 78 passing Node tests include 35 existing favorite/panel regressions and
43 music-source tests: canonicalization, positive Spotify/radio setup, exact
levels, immutable reviews, changed membership/sources, duplicate dispatch,
lost replies, version-1/2 recovery, journal failures, static asset checks and
real loopback HTTP fixtures. The existing workflow runs all three Node suites.

The 22 passing Chromium component cases include the original 12 cases and
10 additional cases covering each Spotify content type, radio, source-mode
switching, optional levels, explicit silence, legacy journals, edited settings
and lost replies. Review and form layouts were checked at 1440, 768, 390 and
320 pixels, including long source text. They use the actual component and DOM
helper with fixture transport and storage, not a live Rust daemon.

Full-page browser navigation is blocked in the editing environment, including
an intercepted fixture origin, so real session-storage reload behavior remains
unverified here. Component remount recovery and the Node HTTP fixtures did run.
No Rust compiler or physical Sonos devices were available: full workspace
compilation, actual scheduled playback, S1/S2 interoperability and the complete
native remote remain acceptance gates. Backend scheduling, scene execution,
policy, asset delivery and speaker protocols are unchanged.
