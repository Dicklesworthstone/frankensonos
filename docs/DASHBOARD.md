# Listening Room

The dashboard at `/dashboard` brings FrankenSonos's own strengths into the
listening experience: household-aware control, a DJ that explains its choices,
whole classical works, preference feedback, and named house scenes. The full
remote remains at `/` for queue editing, grouping, moves, announcements,
schedules, sound settings and other advanced controls.

## Open it

Update the checkout, rebuild, and start the daemon:

```sh
cargo run -p fsonos-cli -- serve
```

Open the HTTP address printed by the daemon, then choose **Listening room &
visualizer**, or append `/dashboard`. It runs in your local browser using the
same listener, native HTTP API, policy and action log as the rest of FrankenSonos.
Tailscale Serve uses the same routes and existing Host/Origin protection. There
is no new service to expose and nothing is installed on the speakers.

All HTML, CSS and JavaScript are embedded. No frontend build, CDN, external font,
analytics or third-party browser dependency is required.

To explore without Rust, a running daemon or speakers:

```sh
python3 scripts/dashboard-preview.py
```

Open `http://127.0.0.1:8765/dashboard?demo=1`. The preview binds loopback only and
has no speaker API. A persistent banner identifies its fictional rooms and
music. You may select groups, inspect DJ factors, explore moods, search fixture
music and review scenes, but confirmation and all playback mutations remain
disabled. Preview data never substitutes for a failed live read.

## A house in harmony

The light appearance uses ivory, sage and forest green; the appearance button
switches to the dark palette. Only appearance and visualization choice are
saved in local browser preferences, not music, room names or audio.

The central stage shows the selected group's actual track, or the whole work
when the DJ's reported movement agrees with the observed track title. A late
DJ read cannot replace a newer source's title. Recording art is read through the
existing `/art` endpoint; the sculptural field and small graphic placeholders
are original procedural art, not purported album covers.

**Your house** shows actual playing groups, their household and their other
member rooms. The selectable **Map** is a grouping diagram, not an invented
floor plan or geographical location. S1 and S2 stay separate. Missing bonded
players and stale observations are labelled. Selecting a group changes the
scope of the player, DJ and search; it never changes physical grouping.

The persistent player keeps pause/resume, previous/next track and group-volume
steps of five within reach, including on mobile. The current group is named
next to the track. Policy clamps and native response notes are displayed.
Elapsed time is estimated from the most recent sample, marked accordingly;
unknown timing is not invented, and stale samples do not keep advancing.

## The DJ, not just a play button

**Behind the pick** shows the actual engine explanation, the current work and
its selection factors. A factor of 1000 is displayed as **×1.0**, neutral;
above one favors a work, below one disfavors it. The displayed number is the
engine's multiplier, not a confidence or taste percentage. Bars are capped at
×2 visually but their labels retain the actual value. Relaxed filters and
reported feed problems remain visible. Owner preferences take precedence over
learned feedback and raw taste signals; the dashboard does not invent scores.

For a multi-movement recording the stage shows **Every movement. In order.**,
the current movement and total, with a segmented work progression. Upcoming DJ
picks distinguish songs from whole works. They are the engine's next selections,
not a fabricated representation of the entire speaker queue.

**Set the feeling** is populated from the daemon's mood catalog, including the
owner's custom presets. Choose a mood and a duration of 30 minutes, one hour,
two hours or until cleared; review the exact group before confirming. Starting
a stopped DJ uses `/dj/start`; changing a running DJ uses `/dj/steer` and shapes
the next pick, not the movement playing. A mood steer replaces that group's
session overrides; standing preferences remain in force. **Follow program**
clears session steering so the time-of-day program can take over again.

**More like this** and **Less like this** record native work feedback after
checking the current work and playback again. They do not skip, ban or change
the owner's saved Spotify library. **Skip whole work** uses `/dj/skip`, leaving
the remaining movements of the current pick; the player's ordinary Next remains
`/next`, a different operation. Session start/stop, mood changes and feedback
all use the existing daemon implementation rather than a second DJ engine.

## Collection and house scenes

Use **Your collection**, the search field, or **Command/Ctrl+K** to search the
cached library and the selected household's favorites. Results are explicit
tracks, albums, playlists or native favorites. Review a specific result before
playing it. The dashboard rechecks that exact identity; it never substitutes a
new top search result. Album and playlist playback can replace the selected
group's queue. Unsupported kinds remain read-only.

**Scenes** lists real saved house states. A review shows their groups, sources,
playing state, per-room volumes and mutes before any change. The dashboard
re-reads both the saved definition and the household topology before invoking
one native scene-apply call. Edited scenes or changed grouping invalidate the
review. The daemon applies its normal policy, logs the action and supplies its
normal undo behavior.

Scenes are multi-step operations, not atomic house transactions. A partially
completed response shows failed and skipped steps; successful earlier steps
are not silently rolled back and no retry is automatically sent. Use the full
remote to inspect the action history and available undo.

## Sculptural visualization

Three distinct procedural Canvas modes:

- **Sculpture:** a lit, slowly deforming three-dimensional ribbon with fine
  copper orbit lines.
- **Halo:** a ridged circular field with different geometry and depth.
- **Terrain:** evolving projected topographic contours.

**Enter the visualizer** expands the composition and requests browser
fullscreen from that user gesture. A full-window layout remains available when
fullscreen is blocked. Back or Escape restores the dashboard and focus. Other
controls are inert while immersed; keyboard focus stays inside the stage.
The current track and transport controls remain available there.

The operating system's reduced-motion preference starts the visualizer still.
**Resume motion** enables animation explicitly. Hidden pages stop drawing;
rendering is paced at at most 24 frames per second with approximately 1.6 million
backing pixels and a capped pixel ratio. Canvas failure leaves the rest of the
interface usable. Ambient geometry responds to selected track identity and
observed playback state, but is explicitly **not measured speaker audio**.

### Optional, private audio reaction

The microphone menu enables real local Web Audio analysis only after an explicit
click and browser permission. It analyzes this computer's default input, not a
direct Sonos feed. Any sound reaching that microphone can influence the visuals.
No recording, upload, persistent audio storage or audio output connection is
created. Live microphone and ambient modes are labelled separately.

Stopping, hiding/leaving the page, or disconnecting the input releases capture.
It never resumes automatically. Cancelled and timed-out permission requests
also fence late grants so they cannot leave a microphone open. A compatible
browser on localhost or HTTPS is required; ambient mode needs no permission.

## Control and lifecycle boundaries

Transport, group volume, DJ, library playback and scenes share one dashboard
mutation lane. Double clicks and competing controls cannot overlap their
preflight and dispatch. Reviews bind the exact household-qualified group,
current playback, applicable DJ work/session, catalog mood or saved scene.
Cancelling, switching group or hiding the page invalidates unsent work.

These checks are read-before-write, not physical-speaker reservations. Another
controller can still intervene between observation and command. The native API
remains authoritative. One action sends at most one mutation; no timeout,
refusal, malformed acknowledgement or lost reply triggers automatic replay.
An uncertain result stays blocked through passive refreshes until the user
explicitly refreshes current state. Accepted commands and observed playback
are distinguished.

One SSE connection coalesces events without starving refresh during event
bursts. Playback reads have bounded four-way concurrency; a 15-second fallback
backs off up to a minute on failure and honors Retry-After. Reads have a ten-second
connection/body deadline; DJ and scene writes allow up to ten minutes without
turning a browser timeout into a playback cancellation. Hidden pages stop new
reads, clocks and SSE and reject stale results from their previous lifetime.

## Validation

```sh
node --test scripts/test-dashboard-session.cjs
python3 scripts/test-dashboard-browser.py
python3 scripts/test-dashboard-preview.py
```

The browser suite requires Python Playwright and Chromium. Pass
`--browser-executable /path/to/chromium` to select an installed executable.
It runs the production assets with explicit synthetic API fixtures, not a Rust
daemon. `--screenshots-dir /path/to/output` captures the rendered preview.

In this redesign session, 34 Node tests passed, including actual loopback HTTP
scene dispatch, native request bodies, immutable/stale reviews, whole-work
feedback, partial scenes, shared control exclusion, cancellation and uncertainty.
Seventeen Chromium acceptance groups passed, including actual production event
handlers, both households, search-to-play, continuous event bursts, safe metadata,
offline recovery, hidden-page shutdown, preview isolation, three rendered modes,
dark/light appearance, immersive focus, microphone fallback and eight viewport
widths from 320 to 1536 pixels without horizontal overflow or page exceptions.
The preview HTTP test checks the real embedded bundle and refuses POSTs.

No Rust compiler or physical Sonos devices were available in this environment.
Rust workspace gates, real daemon/GENA integration and live-speaker acceptance
remain separate checks. Browser policy blocked native fullscreen and actual
microphone capture; the full-window fallback was exercised, not a claim of
native fullscreen or acoustic validation. Source control commits are not evidence
that those unexecuted integration gates passed.
