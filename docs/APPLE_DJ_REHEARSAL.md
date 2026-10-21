# Native DJ rehearsal

Choose a group and open **Your DJ → Preview a DJ set** on iPhone, iPad or Mac.
This is a hypothetical set from the daemon's cached library and that group's
history. Generate does not start music, change the queue, save steering or
preferences, or record listening history. A stopped DJ can be previewed too.

Choose 1–500 works or 1–48 hours, a variation seed, and either now or a house-clock
start time. The hours option includes whole works that start inside its window,
up to the daemon's 500-work cap; the final work may finish after the window.
HH:MM means the next occurrence on the daemon's house clock, not the phone's
current time zone. Results keep the daemon's date, time and UTC offset visible.
Seeds are decimal UInt64 values, including values above JavaScript's exact-integer
range; they are never rounded through floating point.

With no preview overrides, the rehearsal follows the group's current steering,
its expiry and the time-of-day program. An optional mood or filter replaces that
steering only within the preview. The editor exposes artist, genre, keyword,
composer, period and release-decade inclusions/exclusions, work lengths, energy
shift and long-work permission. Lists use one value per line; commas remain part
of a name. Standing taste remains applicable. The daemon decides which filters
must relax and supplies the selection reasons; the client never invents picks.

## Results, export and lifecycle

Results show each whole work in order, its planned house time, artist/composer,
performers, movement count, known duration, genre, explicit flag, energy target,
mood and selection reasons. Expand **Why this pick** for the daemon's weighted
factors. Hourly summaries show its artist/genre entropy and mean energy. Empty
sets are shown as empty, not filled in with recommendations from another source.

**Share this preview as text** exports the generated observation through the
platform share sheet. It does not apply the preview or change the current draft.
The text labels the result as hypothetical and includes its group, start, seed,
work order and reasons. A later live session can differ as time, library, history
and preferences change; a seed alone is not a playlist reservation.

Only **Generate preview** initiates a rehearsal. Expanding the panel, editing,
changing tabs, or foregrounding the app does not generate one automatically.
One preview read runs at a time; cancelled work must drain before reuse. Edits,
group changes, collapse, backgrounding and disconnection cancel obsolete work.
Late results cannot overwrite newer choices or follow a same-named room into
another household. A retained older result is labelled **Previous preview**.

The client uses the existing endpoint and bounded URLSession transport. It reads
current topology before a single JSON POST to `/dj/rehearse`, confirms the group,
and validates the response's seed, scope, timing and totals. These are read-only
observations, not atomic reservations. Failures retain their API/read error;
they are not described as uncertain playback writes, and no retry is automatic.
No new endpoint, credential, Spotify scope or dependency is introduced.

## Verification

Run the complete package and Apple build gates with the repository workflows:

```sh
swift test --package-path apps/apple -Xswiftc -warnings-as-errors
```

The new client/model and draft tests were executed under Swift 6.2.1/Linux with
warnings as errors in a focused harness: exact production rehearsal/model/parser
files and verbatim relevant client, transport and model declarations. Unrelated
playback/account writers were excluded; this is not a full-package result. The
immediate-cancel regression fails against the previous rehearsal model.

A real loopback HTTP fixture also exercised the production URLSession read path:
valid generation, API error, changed group, malformed response and refused HTTP
redirect. Exactly five topology GETs and four rehearsal POSTs were observed,
with no playback endpoint request or automatic retry. These are synthetic daemon
responses, not a running Rust engine. Both iOS17/macOS14 view branches passed
syntax parsing, and the Xcode project passed plist/reference checks. Full Apple
SDK type-checking, rendered native UI/share-sheet behavior, full-package tests,
Rust integration and physical Sonos acceptance remain separate, unverified gates.
