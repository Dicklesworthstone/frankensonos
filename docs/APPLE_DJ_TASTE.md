# Standing DJ taste on iPhone, iPad and Mac

Open **Your DJ → Your standing taste**. The settings belong to every DJ on the
connected daemon, not the selected speaker, household or Spotify account.
They are available without a selected or reachable speaker. Opening the panel
reads the existing `GET /dj/preferences`; it never starts a session or music.

## A lasting preference, not a temporary steer

Use **Favor** and **Avoid** for genres, artists/composers, eras and moods.
Use **Pin** and **Ban** for artists, albums and tracks. Each addition is one
literal value: commas remain part of a name. The daemon remains the authority
for music matching, normalization, valid eras/moods and conflicting entries.
The saved-list filter works locally; removing an entry names that exact
observed entry. There is no implicit clear-all operation.

Energy can follow the time-of-day program or be a standing 0–100 musical-energy
preference; it never changes speaker volume. Explicit-content choice affects
future DJ picks. Favor increases weighting, avoid may be relaxed when needed,
pin strongly favors a particular item and ban is not automatically relaxed.
Temporary explicit room steering takes precedence over standing taste.
None of these operations skips a work, reorders existing music or restarts
playback. Speaker Undo does not restore standing preferences.

Sliders, switches, text and category selection are local drafts. **Review**
reads the complete preferences again, then shows the exact one-key operation
and its daemon-wide scope. **Save for every DJ** checks again and sends one
`POST /dj/preferences` with `key`, `value` when relevant, and `unset`. It never
replaces the preference document or fabricates a zone. A changed observed
baseline requires a new decision. Read-before-write is not an atomic lock
against another client or a file edit after the final read.

## Native ownership and recovery

Preference saves use the connection's existing `CommandReviewSession`, shared
with playback, scenes, schedules and undo. The native model blocks competing
reviews while a preference operation owns it and waits for cancellation to
drain before reuse. Topology changes do not retarget a daemon-wide review.
The panel's reader is single-flight with obsolete-response fencing, no timer
and no additional event stream. Collapsed panels do not request preferences.

Leaving or backgrounding invalidates unsent work. A submitted write may still
finish; its result stays attached to the exact preference, not the latest room
selection. Lost or contradictory replies are explicitly uncertain and are not
retried. Reopening and passive reads do not clear uncertainty. Use **Refresh
standing taste** to read current settings before deciding on another change.
A failed read retains the original observation time, not edit authority.
Missing fields or an unavailable file never become invented defaults.

The native controls use system text styles, accessible named controls, a
searchable saved-entry list and a scrolling review with persistent actions.
Titles and names wrap; a removal is always labelled with the entry and list.
The same shared view is included in the checked-in iOS project and Mac package.

## Validation boundaries

`swift test --package-path apps/apple --filter DJPreferences -Xswiftc -warnings-as-errors`
executes the preference document, local editor, production-client/review and
workflow tests. Controlled transport supplies synthetic daemon responses;
these tests do not exercise real Spotify or Sonos equipment. Apple CI builds
the real SwiftUI products. Its connection-screen simulator launch is not a
connected taste-screen visual or VoiceOver acceptance test.
