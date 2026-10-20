# Native iPhone, iPad and Mac remote

`FrankenSonosRemote` is a SwiftUI application for iOS/iPadOS 17 and macOS 14
over the existing daemon API. The iOS app and Mac executable use the same
application sources and the local `FrankenSonosKit` package, including one
command-review model and live-state connection. Its connection screen accepts
an HTTPS daemon origin, including your Tailscale Serve origin. Local HTTP
requires an explicit toggle and the same local-address validation as
`DaemonClient`. Only that origin and toggle are saved in the app's preferences.
A saved valid daemon address reconnects automatically on launch by reading
state; opening the app never starts music or replays a previous command.

## Run on an iPhone or iPad

Open the checked-in app project with Xcode 16 or later:

```sh
open apps/apple/FrankenSonosRemote.xcodeproj
```

Choose the **FrankenSonos-iOS** scheme and an iPhone or iPad simulator, then
Run. The app target links the `FrankenSonosKit` product from `apps/apple`;
there is no second copy of the networking or control code to synchronize.
No project generator or third-party package dependency is needed.

To install on your own iPhone or iPad, select it as the run destination and
choose your Apple development team under the app target's **Signing &
Capabilities**. Xcode manages the development provisioning. Keep your signing
team and provisioning files local. On your device, connect to the same tailnet
as the daemon and enter its Tailscale Serve HTTPS origin. A simulator's
`localhost` means the Mac running the simulator; on a physical phone it means
the phone itself.

For an unsigned simulator build from the repository root:

```sh
xcodebuild -project apps/apple/FrankenSonosRemote.xcodeproj \
  -scheme FrankenSonos-iOS -configuration Debug \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath apps/apple/.build/ios \
  CODE_SIGNING_ALLOWED=NO SWIFT_SUPPRESS_WARNINGS=NO \
  SWIFT_TREAT_WARNINGS_AS_ERRORS=YES build
```

The resulting app is
`apps/apple/.build/ios/Build/Products/Debug-iphonesimulator/FrankenSonos.app`.
It is a simulator build; device installation uses Xcode development signing.
The checked-in icon and adaptive launch background are included in the app.
The app supports portrait, landscape and iPad resizing, and uses the system's
light or dark appearance.

## Run on a Mac

With Swift 6 and macOS 14 or later, from the repository root:

```sh
swift run --package-path apps/apple FrankenSonosRemote
```

For a standalone local application bundle:

```sh
bash apps/apple/package-remote.sh
open apps/apple/.build/FrankenSonosRemote.app
```

Packaging refuses an existing destination rather than deleting it. Supply a
new bundle path as the script's first argument for another build. The bundle is
ad-hoc signed, not Developer ID signed or notarized. It declares why it accesses
the local network and permits local networking through App Transport Security;
the client still rejects public HTTP, embedded credentials and redirects. No
system-wide security setting is changed.

The **Native Apple remotes** workflow tests the full Swift package on a macOS
runner and packages the Mac product in release mode. Its independent iOS job
builds the actual app target with Apple SDKs and warnings as errors, launches
its connection screen on an available iPhone simulator, and saves a screenshot,
the simulator app and the Xcode result bundle. The connection screen needs no
daemon or speaker. These jobs run independently of the Rust release build.

### Local network access

Both app bundles declare the reason for local network access. Their App
Transport Security settings permit local names and the same loopback/private
IPv4 ranges accepted by `DaemonEndpoint`, plus IPv6 loopback. This lets the
explicit local HTTP choice work on iOS 17 and macOS 14, where numeric hosts
require [address or CIDR exceptions](https://developer.apple.com/documentation/bundleresources/information-property-list/nsapptransportsecurity/nsallowslocalnetworking).
Public HTTP remains rejected by the client; HTTPS is the tailnet connection
path. No multicast entitlement is required because discovery stays on the
daemon and the app connects to the origin you provide.

## What the screen connects

The app has four destinations: **Listen** for artwork, playback and room levels;
**Music** for favorites, library browsing/search and the queue; **Your DJ** for the current
whole work, explanations and steering; and **House** for scenes, sleep timers,
schedules, grouping, Spotify setup and activity. A mini player remains above the
tab bar while browsing. The Mac keeps a room sidebar; the iPhone and iPad use a
searchable room chooser. Both retain household names when room names overlap.

Artwork loads through the selected daemon's art endpoint. Playback progress uses
the selected room's own observation timestamp, so an unrelated room update cannot
reset its clock. The estimate stops advancing when the connection is stale and
unknown durations remain explicit. Seek and volume sliders build a draft; the
app presents the existing command review before sending any change. Reviews
scroll independently of their persistent confirmation controls on small screens.

Rooms are grouped by household. Selection anchors to a household-qualified room
and follows its group if a coordinator changes. The screen shows playback,
reported track details, member levels and mute, group levels, unavailable-room
errors, and the freshness and time of the last house observation.

Playback controls include pause/resume, previous/next and absolute seeking when
a source reports a seekable duration and position. Sliders choose targets only;
they never send while dragged. Grouping menus join or separate rooms, group a
household, move playback, or request a cross-household replay with the existing
backend's semantics (not synchronized playback).

The queue is paged in groups of 25. Playing or removing an entry carries its
exact URI and revision through fresh preflight and the daemon's native queue
checks. Pages are not concatenated across revisions. A missing revision leaves
the page readable but not editable. Source URIs and library results can be
played, queued next or appended; adding to the queue does not start playback.
Favorites are read from the selected household and reviewed by their stable ID.

The library browser opens saved albums, tracks and playlists without requiring
a selected or reachable speaker. Album cards open their saved track lists;
filters and pagination stay tied to one cache revision. A changed cache requires
a fresh listing before selection, so old rows cannot silently become new music.
Play, queue next and append all use the selected household/group's existing
command review. Album playback uses the full album source; the displayed track
list describes cached coverage, not a promise that every album track is saved.
**Search all** keeps the combined cached-library and household-favorites search.
Spotify sign-in and library synchronization are available from **House**.

The DJ panel shows its current whole work, the upcoming works and the reason for
its choice. It connects start, stop, whole-work skip, like/dislike, timed mood
steering and clearing steering. Failure to read the mood catalog does not erase
an otherwise valid running DJ observation. Favorites, queue, DJ and library
reads have local errors; a failed panel does not prevent a fresh playback review
on a reachable room. Panels refresh on selection, explicit refresh, relevant
live-state transitions and completed commands; they are observations, not an
independent event log.

The Scenes panel browses and applies stored house definitions with a full
settings review and explicit house-wide scope. Its results preserve failed
steps, skipped rooms and policy notes; creating and editing scenes remains on
the CLI/web. See `docs/APPLE_SCENES.md` in the repository.

The Sleep panel reads the selected group's timer and supports setting a new
one, extending its existing deadline, and cancelling it. It distinguishes daemon
fades from speaker-only timers. The displayed countdown is an estimate, not an
acknowledgement that playback paused. Timers continue on the daemon/speaker when
the remote closes; no offline command queue or local alarm is created. See
`docs/APPLE_SLEEP.md` in the repository.

The Schedules panel creates weekly or explicit-offset one-time schedules for
DJ starts, faded pauses, room volumes and saved scenes. It also lists, pauses,
resumes and removes existing definitions, including when speakers cannot be
read. Draft targets are explicit, never borrowed from a changing playback
selection. The daemon owns run times, persistence and creator permissions; the
remote adds no scheduler. See `docs/APPLE_SCHEDULES.md` in the repository.

The **Spotify & Library** panel opens daemon sign-in and cache refresh without
a selected speaker. Browser consent is opened explicitly, completion is sent
once, and credentials stay on the daemon. Cached sign-in, job acknowledgement,
completed imports and missing-data notes are shown separately. The setup panel
owns no playback commands; its pending-job observations are GET-only. See
`docs/APPLE_SPOTIFY.md` in the repository.

The **Activity** panel keeps action history and listening history available,
including filtering and observed-state freshness. A historical play can be
selected for replay or queueing into an explicitly chosen current group;
playback still goes through the ordinary command review. Historical room names
never silently choose the destination, and browsing history sends no playback
commands.

Activity also offers **Review available undo** for the daemon connection's
authenticated caller. It does not infer eligibility from log rows or use the
selected room or history filters to choose the target. Review shows the original
action, caller, time and result before any restore request. Confirmation rereads
the exact candidate and sends its full-width ID once to `/undo/checked`; the
daemon controls eligibility, affected rooms, policy and intervening-state checks.
Partial restores retain every reported failure. An uncertain response is never
retried, and a late receipt stays associated with its original action across
room changes or suspension. Undo remains usable without a selected speaker.

## Commands and lifetime

Playback, schedule and undo mutations go through `RemoteModel` and
`CommandReviewSession`: read, show a one-use review, explicitly confirm, read
again, then dispatch once. Spotify account/cache operations use an independent
single-shot setup worker and never send speaker commands.
The confirmation shows the source household, group, requested target or URI,
current playback, and the relevant favorite or DJ selection. Room and group
levels, queue entries, playback identity, DJ steering, and affected group layouts
are checked according to the command. Clock progress alone does not invalidate
a seek. These client checks are not atomic reservations against other controllers;
the daemon remains authoritative for policy and device operations.

A double confirmation cannot dispatch twice. Changing rooms or suspending the
window cancels an unsent review and fences delayed reads. A cancelled operation
keeps its slot until it drains, so a new command cannot race it. A sent command
is not undone by cancellation. Its receipt or uncertain-outcome message stays
attributed to the original target, including the actual member for a room-level
change, even when the selection changes. Refresh never turns an uncertain write
into a confirmed success and never replays it. Disconnect is disabled while a
confirmation is in progress; closing the application can still lose an in-flight
receipt, and the application does not persist or retry that command on restart.

The existing `LiveHouse` owns reconnects, event coalescing and background/foreground
state. A stale display is explicitly marked; any new action must pass fresh
command reads. Queue and other panel requests are separately cancellation-fenced.
One main-actor UI adapter observes `RemoteModel`; the controller itself has no
SwiftUI, Combine or Observation runtime dependency.

## Verification and remaining acceptance

```sh
swift test --package-path apps/apple -Xswiftc -warnings-as-errors
swift build --package-path apps/apple --product FrankenSonosRemote -Xswiftc -warnings-as-errors
xcodebuild -project apps/apple/FrankenSonosRemote.xcodeproj \
  -scheme FrankenSonos-iOS -configuration Debug \
  -destination 'generic/platform=iOS Simulator' \
  CODE_SIGNING_ALLOWED=NO SWIFT_SUPPRESS_WARNINGS=NO \
  SWIFT_TREAT_WARNINGS_AS_ERRORS=YES build
```

Nineteen command-session tests and thirteen screen-controller tests execute the
production client, session actors and screen model through controlled transports.
They include positive playback, member-volume, queue-removal, favorite and DJ
feedback commands, as well as changed identities, cancelled preflight, slow writes,
late acknowledgements (including topology events before a grouping reply),
duplicate confirms, failed panels, and suspended/stopped
sessions. The two client regressions were also reproduced against the original
source: real feedback was incorrectly decoded as an Outcome, and cancellation
inside the final topology read could still send a POST.

The scene and sleep workflows add twelve scene tests and twenty-two sleep tests
using the production codecs, command actors and screen controller with controlled
daemon responses. They also run in the full package on the native Mac workflow.

Schedules add twenty-one client/command tests and seventeen form/screen tests.
These cover all four action types, store management without speaker reads,
house-local and offset-explicit times, shared playback command locking, cancelled
reads, and uncertain acknowledgements that must not be replayed. Schedule-list
observations are not evidence that a scheduled action actually ran.

Spotify setup adds twenty-eight client/workflow tests for read-only consent,
callback binding, explicit browser opening, partial imports, read isolation,
lifecycle cancellation and uncertain requests without replay. They use synthetic
daemon replies, not a real Spotify account grant.

Local Linux execution does not validate SwiftUI rendering, Apple application
launch, Apple URLSession streaming, Tailscale reachability or physical Sonos.
The Apple workflow's app builds and iPhone connection-screen launch cover the
SDK and packaging boundaries; connected playback and visual acceptance on
physical devices remain separate checks. This remote does not yet expose every
daemon feature: scene editing, announcements, EQ and action-log undo remain on
the CLI/web surfaces. App Store/TestFlight distribution is not configured.
