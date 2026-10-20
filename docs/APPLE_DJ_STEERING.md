# Native temporary DJ steering

In the native remote, select a group and open **Your DJ → Temporary session
filters**. Refresh the DJ to load its current temporary additions, then choose
an optional base mood and filters. Artist, genre, keyword, composer and period
fields take one name per line; commas stay inside a name. Decades use their
first year, such as `1960` or `1980`. Each list holds at most 32 entries.

The editor exposes inclusion and exclusion for all six list categories, optional
minimum and maximum work lengths (1–600 minutes), an energy shift from -2 to +2,
and permission for long works. Classical period filters are separate from release
decades. The daemon remains responsible for library matching and reports filters
it relaxes when too few works qualify; the client does not invent selections.

**Review replacement** sends nothing. The existing native command sheet shows the
exact group, mood, additional filters, and duration before confirmation. Saving
replaces the entire temporary session, not just one edited field. A duration of
1–10080 minutes starts when saved; **Keep until explicitly cleared** omits a
finite duration. This editor does not preserve an old expiry automatically.
**Review clearing all temporary steering** explicitly returns to the automatic
program. Neither action starts a stopped DJ, replaces the queue, nor changes
standing taste preferences.

Only the session's `added` filters are imported into a draft. Mood/program
constraints are not silently promoted into session additions. Unknown filters,
malformed values and ambiguous mood names prevent replacement rather than being
discarded. Changed groups require new editing; drafts never follow a different
household merely because room names match.

`NativeCommand.djFilters` reuses `CommandReviewSession`, the existing `/dj/steer`
endpoint, and `RemoteModel` result handling. Preparation and confirmation both
read the edited session and, for a named mood, its definition. Changed steering,
expiry or mood definitions stop unsent work. A decreasing countdown and advancing
track do not by themselves invalidate a filter edit. These observations are not
an atomic reservation against another controller. The shared one-use write path
never retries an uncertain save or turns a filter save into a playback command.

## Validation

Run the complete native package with:

```sh
swift test --package-path apps/apple -Xswiftc -warnings-as-errors
```

Eight decoder and sixteen filter/draft tests pass with warnings treated as errors
under Swift 6.2.1/Linux in a focused package of exact production source files.
The decoder tests include the daemon's omitted-empty-queue shape; six fail against
the previous decoder. Six additional command-session tests cover exact writes,
changed baselines, mood drift, track progress, cancelled reviews and uncertain
acknowledgements; they are committed but have not been executed locally.

Changed integration code and both iOS17/macOS14 view branches pass syntax parsing.
The Xcode project includes the panel in the iOS sources. Full-package integration,
Apple SDK type checking, rendered native UI and physical Sonos acceptance remain
separate checks; syntax parsing and the focused tests do not establish them.
