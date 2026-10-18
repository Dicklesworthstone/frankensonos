# Native Mac remote

`FrankenSonosRemote` is a SwiftUI macOS application over the existing daemon
API. It does not launch another daemon, talk directly to speakers, learn new
Spotify credentials, or implement a second DJ. Its connection screen accepts an
HTTPS daemon origin, including your Tailscale Serve origin. Local HTTP requires
an explicit toggle and the same local-address validation as `DaemonClient`.
Only that origin and toggle are saved in the app's preferences.

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

The **Native Mac remote** workflow separately tests the full Swift package on a
macOS runner, builds the real SwiftUI product in release mode, and uploads an
application archive with these notes and its architecture. It does not wait for
the Rust release build. A successful Linux build is not evidence of this Mac
build: the Linux executable only explains that the UI requires macOS and exits
with status 1.

## What the screen connects

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

The DJ panel shows its current whole work, the upcoming works and the reason for
its choice. It connects start, stop, whole-work skip, like/dislike, timed mood
steering and clearing steering. Failure to read the mood catalog does not erase
an otherwise valid running DJ observation. Favorites, queue, DJ and library
reads have local errors; a failed panel does not prevent a fresh playback review
on a reachable room. Panels refresh on selection, explicit refresh, relevant
live-state transitions and completed commands; they are observations, not an
independent event log.

## Commands and lifetime

Every mutation goes through `RemoteModel` and `CommandReviewSession`: read,
show a one-use review, explicitly confirm, read again, then dispatch once.
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

Local Linux execution does not validate SwiftUI rendering, macOS application
launch, Apple URLSession streaming, Tailscale reachability or physical Sonos.
Those remain separate Mac/workflow and owner-LAN acceptance checks. This remote
does not yet expose every daemon feature: scene editing, schedules, sleep timers,
announcements, EQ, Spotify sign-in/sync and action-log undo remain on the CLI/web
surfaces. It is a runnable Mac target, not an iOS installation or App Store release.
