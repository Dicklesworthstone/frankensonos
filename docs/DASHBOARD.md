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

Choose a position on the persistent player's timeline, or use its **Seek within
track** button on desktop and mobile. The position dialog supports keyboard
scrubbing and 15-second steps, shows the exact track and listening group, and
sends one native seek only after confirmation. Seeking a paused track leaves it
paused. Streams without a known duration, stale observations and the design
preview cannot seek. A changed group, track, duplicate queue occurrence or
play/pause state discards the review. A fresh state read checks the same identity
immediately before dispatch; the speaker does not provide an atomic seek
precondition. An unconfirmed request is never retried automatically.

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

## Save the current house and its queues

**Save current house** in the scene section captures every listening group, its
source and playback state, and each room's volume and mute. Enter a name, review
the house groups, then save through the native scene API. Capture reads the
house when confirmed and changes only saved storage; playback continues as it is.

By default, ordinary queues become bookmarks that retain their occurrence,
position and known playback mode. Recall requires the original players and
unchanged queues. Enable **Back up complete queue contents** to retain the
ordered tracks too, up to 1,000 entries and 4 MiB per queue. Recall can then
reconstruct those queues on their original players after queue edits or restarts.
DJ sessions retain their direction rather than a frozen queue copy. Unsupported,
changing or oversized queue captures fail in the daemon before replacing a saved
scene.

An existing name opens an explicit replacement review with its current saved
groups, sources and room levels. The stored spelling is preserved for case-only
matches. Confirmation rereads the saved definition and whole-house grouping;
changed definitions, occupied names, stale observations or a closed/hidden view
retire the review. These checks do not lock other controllers out between the
preflight and native capture.

After saving, the new scene appears in the scene cards and **Review recall**
opens the existing whole-house apply review. Saved queue backups state their
entry count and occurrence and explain that recall replaces current queue
contents. Replacing a saved definition and restoring backed-up queue contents
cannot be reversed through speaker action-history undo.

Capture and recall share the dashboard's single mutation lane. A lost or
unreadable save reply keeps the outcome uncertain and requires explicit
**Refresh**; it is never retried automatically. Scene reads are on demand, and
the design preview cannot save.

## DJ selections and the actual speaker queue

The Up next panel has two keyboard-accessible tabs:

- **DJ selections**: the engine's upcoming whole works or songs, not a claim about
  the entire Sonos queue.
- **Speaker queue**: native `/zones/{room}/queue` pages, forty entries at a time,
  showing total count, exact positions and the reported playing entry. Duplicate
  URIs remain separate tracks. The native editor reviews playback, removal, and
  moves of one entry or a consecutive range; each command rechecks its revision
  and exact renderer identities before dispatch.

Queue reads are lazy: no requests until the tab is opened. Pages are replaced,
never combined across revisions. A removed tail page triggers at most one read
back at the start. Failed or incomplete replies retain the prior page explicitly
as stale and disable pagination until it is read again. The existing live-event
refresh invalidates visible queue observations without adding another stream or
a queue polling loop. Only one queue read can be in flight. Closing the tab,
changing groups, hiding the page or entering immersive mode fences late replies.
Read-only queue inspection cannot replay a command or mutate playback.

### Find and navigate a long queue

**Find or jump in this queue** searches titles, artists and albums through the
native `GET /zones/{room}/queue/search` endpoint. One explicit search returns at
most 50 matching occurrences and shows the exact range examined. **Search next
section** continues with the returned offset and the same queue revision; each
section replaces the previous results so large queues do not grow browser memory
without a bound. Matching and the scan budget remain the daemon's responsibility.

**Show in queue** rereads the result's forty-entry page and verifies its position,
metadata, renderer URI, revision and queue length. Repeated tracks remain separate
occurrences. A changed result becomes stale and cannot select or play a different
entry. **Show position** jumps to a known absolute position, and **Show playing
#…** opens the last reported playing position. These controls open pages without
changing playback; the existing queue controls review any subsequent action.

Search shares the queue's single read lane, can be cancelled, and is retired by
query edits, group changes, live invalidation, closing the queue or hiding the
page. A slow bounded search gets a 60-second request deadline while ordinary
dashboard reads keep their 10-second deadline. No browser polling or automatic
continuation is added.

### Shuffle and repeat

**Shuffle & repeat** in Up next reads the selected group's actual queue mode on
demand. The dialog offers in-order or shuffled playback, with no repeat, queue
repeat or current-track repeat. It shows the current setting and applies only the
reviewed change through the native API. The queue remains selected and playback
does not restart.

A stream or other non-queue source cannot be switched by this control. The DJ
keeps whole works in sequence, so its active session blocks shuffle and repeat.
Changed grouping, queue/DJ ownership or an intervening mode change invalidates
the review. Reads expire after 45 seconds; hidden pages and closed dialogs retire
unfinished reads. An unconfirmed write is never retried automatically.

### Sleep timers

**Sleep timer** in Up next reads the selected group's current timer on demand.
Choose a preset from 15 minutes to two hours, or enter any whole duration from
1 to 1,380 minutes. Review **Set timer** to replace the existing deadline,
**Extend timer** to add time, or **Cancel timer** to stop it. Each review names
the whole listening group. Extending or cancelling during a fade ends that fade;
cancelling does not restart music.

The dialog shows the native deadline in your browser's time zone, an approximate
countdown and whether the daemon is fading or the speaker is using a timer
without a fade. The daemon or speaker owns the timer, so closing the browser does
not cancel it. The displayed countdown advances from the reported remaining
duration; reaching zero asks for a fresh read and does not claim playback paused.
An observation older than 45 seconds remains labelled as a last observation but
cannot authorize a change.

Confirmation rechecks the group, coordinator, current deadline and fade state
before one native `/sleep` command. A changed or expired review, hidden page or
different selection prevents dispatch. Timer changes share the dashboard's
command lane, preserve uncertain outcomes until explicit **Refresh**, and never
retry automatically. One fresh timer read follows an accepted change; there is
no background timer polling. Other controllers can still act between preflight
and the native command.

### Individual room volume and mute

**Room volume** below the room cards opens a mixer for every member of the selected
group. Each room has its own 0–100 slider and explicit mute/unmute control. Moving
a slider only chooses a proposed level; **Review level** names the room and change
before confirmation. Mute affects that room's audio while the group keeps its
current playback. The persistent player's existing ±5 controls still adjust the
group.

The mixer reads physical room state on demand with at most four requests in
flight. Missing volume and mute observations disable their own controls
independently, and an unavailable member does not disable another room's valid
controls. The preflight checks current group membership, coordinator and the
reviewed room's volume and mute. Different households with identically named
rooms remain separately addressed.

Changes use one native room command through the dashboard's shared command lane.
Policy-clamped volume is shown as an accepted actual level, including the native
policy notes, separately from the next observed level. A clamp never triggers
another write toward the requested level. There are no deferred slider writes,
automatic retries, or additional polling. Observations expire after 45 seconds;
selection changes, hidden pages and closing the mixer retire pending reviews.
Unconfirmed writes retain the dashboard's global uncertainty until explicit
**Refresh**.

### Group rooms and move music

**Group rooms** below the room cards can add a room to the selected listening
group, separate an individual room, or play that group's music throughout its
household. The review names the affected groups and explains when current audio
will be replaced or a coordinator may change. Same-name rooms in different
households remain distinct; joining and whole-house playback stay within one
household.

The dialog reads current grouping on demand, then checks the affected membership,
coordinators and reported unavailable rooms again before sending one native
command. A new room also invalidates a whole-house review. Observations expire
after 45 seconds, and changing selection, hiding the page or entering immersive
mode discards the review. All operations use the dashboard's shared command lane
and refresh the whole house after an accepted result. An unconfirmed or partial
failure is never retried automatically; use **Refresh** to inspect current state
before another command. Whole-house grouping can complete only partly.

**Move music** chooses the room the music will leave and its destination within
the same household. The destination joins the source music, then the selected
source room leaves; other source-group rooms can keep playing. Moving within an
existing group separates only the selected source room. Coordinator handoff and
any device fallback use the native move operation.

**Replay elsewhere and stop here** can target another group in either household.
It replaces that group's source or queue and then stops the entire source group.
The households remain separate, and paused music is loaded silently. This option
uses the native replay transfer, including queue and track-position handling.

Transfer reviews read the music in both affected groups and show their playback
state and reported queue occurrence. Confirmation rechecks membership, source URI,
queue occurrence, known duration and transport state in both groups. A changed
duplicate occurrence needs a new review; normal elapsed progress does not.
Unloaded or unsettled sources cannot be reviewed. Topology is checked again after
the audio reads, and an expired review cannot dispatch while preflight is pending.
Other controllers can still act between these observations and the native command.

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
node --test scripts/test-dashboard-sleep.cjs scripts/test-remote-timers.cjs scripts/test-dashboard-volume.cjs
node --test scripts/test-dashboard-scenes.cjs scripts/test-dashboard-http.cjs
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
