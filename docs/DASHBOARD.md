# Listening Studio

The graphical dashboard lives at `/dashboard`; the complete remote stays at `/`.
Both are clients of the same daemon, using its existing policy, action log,
household resolution and speaker protocols. No frontend build, remote fonts,
CDN, analytics, or new cloud account is required.

## Run it

After updating the checkout, rebuild and start the daemon:

```sh
cargo run -p fsonos-cli -- serve
```

Open its printed HTTP address and append `/dashboard`. The same route works
through the daemon's existing Tailscale Serve configuration. Speakers stay on
the LAN; this adds no listener or new control backend.

For a read-only demonstration without Rust, a daemon or speakers:

```sh
python3 scripts/dashboard-preview.py
```

Open `http://127.0.0.1:8765/dashboard?demo=1`. A persistent banner labels the
fictional music and rooms. The preview can explore room selection, search,
scene/mood reviews and a synthetic queue, but cannot send speaker commands.
Its HTTP server binds loopback, serves only the assets and rejects POSTs.
Preview data never replaces a failed live read.

## Listening, not navigating a wall of controls

The artwork-first stage shows the selected recording, observed playback state,
artist, album and household-qualified group. Actual artwork from `/art` appears
in both the stage and persistent player. When art is unavailable, the original
procedural visualization remains visible; it is not represented as album art.

A whole classical work is shown only when the DJ's current movement agrees with
the observed track title. Segmented progress shows that work's current movement.
Ordinary Next advances one track; Skip whole work invokes the native DJ operation.
Unknown track positions remain unknown; estimated elapsed time is labelled and
stops advancing when state is stale.

Your personal DJ shows its real explanation and selection multipliers. These
are not confidence percentages: ×1 is neutral. Mood presets come from the daemon,
including custom presets, and may be applied for 30 minutes, one or two hours,
or until cleared. Follow program clears session steering. Reviewed feedback
shapes future picks without silently skipping, banning or editing Spotify saves.

Scenes show their actual groups, sources, levels and mutes before applying.
Changed scene definitions or grouping invalidate the review; partial outcomes
retain failed and skipped steps. Search uses the cached library and selected
household's favorites, and plays the exact reviewed result rather than a new
first-ranked match. These workflows share one mutation lane with transport and
volume, retaining native policy and action logging.

## DJ selections and the actual speaker queue

The Up next panel has two keyboard-accessible tabs:

- **DJ selections**: the engine's upcoming whole works or songs, not a claim about
  the entire Sonos queue.
- **Speaker queue**: native `/zones/{room}/queue` pages, forty entries at a time,
  showing total count, exact positions and the reported playing entry. Duplicate
  URIs remain separate tracks. No queue writes or speculative item jumps are added.

Queue reads are lazy: no requests until the tab is opened. Pages are replaced,
never combined across revisions. A removed tail page triggers at most one read
back at the start. Failed or incomplete replies retain the prior page explicitly
as stale and disable pagination until it is read again. The existing live-event
refresh invalidates visible queue observations without adding another stream or
a queue polling loop. Only one queue read can be in flight. Closing the tab,
changing groups, hiding the page or entering immersive mode fences late replies.
Read-only queue inspection cannot replay a command or mutate playback.

## Room scope and keyboard interaction

The stage's group pill, the player group name and the Rooms button open a
searchable room chooser. Filter by household or any member room, move through
results with Up/Down, and press Enter to select. Same-name rooms in different
households stay distinct. Selection changes which group is controlled; it does
not move music, regroup speakers or infer a physical floor plan.

Command/Ctrl+K opens collection search, with Up/Down and Enter to review a result.
Escape closes the search or chooser on the first press, including when a search
field contains text, and restores the opener. Home/End navigate results without
hijacking text editing. A shortcuts dialog documents R for room selection,
Space for pause/resume, V for immersion, and Escape to return. Playback shortcuts
are disabled while typing, reviewing, composing input, or inside another dialog.

On mobile, a bottom navigation bar and persistent player keep the selected group
visible. Group volume opens in a dedicated popover; it uses the same native ±5
commands and policy as desktop. Escape returns focus to its trigger. Status
messages do not cover the open volume controls.

## Appearance and visualization

The default is a midnight palette with lilac, warm and teal accents; the appearance
button switches to a light palette. Only appearance and visual mode are saved in
local preferences, not room names, tracks or audio. Existing saved appearance is
respected.

Prism is a smooth software-shaded iridescent field, with a vector fallback for
browsers without OffscreenCanvas. Halo and Terrain retain distinct geometry.
Rendering has a bounded backing store, paced animation and reduced-motion support;
hidden pages stop drawing. Immersive requests browser fullscreen from a gesture,
with a full-window fallback, constrained focus and an explicit exit.

These are ambient graphics, not invented FFT, BPM or direct Sonos measurements.
Optional microphone analysis is local Web Audio after explicit permission. Audio
is never recorded, uploaded, stored or connected to an output. Stop, page hiding,
input loss and cancelled/late permission grants release capture. It never resumes
automatically. Real microphone access requires browser permission and localhost
or HTTPS.

## Reliability and evidence

Playback, DJ, library and scene writes retain their checked native request paths.
A read-before-write check is not an atomic physical-speaker reservation. Commands
are never automatically replayed after failures or lost replies; uncertain outcomes
remain blocked through passive updates until explicit refresh.

Run the local suites:

```sh
node --test scripts/test-dashboard-session.cjs scripts/test-dashboard-queue.cjs scripts/test-dashboard-workspace.cjs
python3 scripts/test-dashboard-browser.py --browser-executable /path/to/chromium
python3 scripts/test-dashboard-workspace.py --browser-executable /path/to/chromium
python3 scripts/test-dashboard-preview.py
```

The browser suites require Python Playwright and Chromium. They exercise the
production assets with explicitly synthetic API responses, not physical speakers.
Use `--screenshots-dir /path/to/output` to capture the rendered interface.
The Node suite covers native payloads, immutable reviews, real loopback HTTP
session dispatch, queue positions, stale/hidden lifetimes and keyboard navigation.
The preview test serves the actual bundle through a real loopback HTTP listener.

This implementation session ran these checks locally; they do not establish Rust
workspace compilation, real daemon/GENA integration, physical-speaker operation,
or actual microphone/fullscreen permission on a user's browser. Those remain
separate acceptance gates. The implementation environment had no Rust toolchain.
