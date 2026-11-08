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

Play/pause follows the observed transport state and remains usable when a
playing or paused source has no track metadata. Next and Previous still need a
known track; unknown or transitioning transport disables these playback buttons.

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

## Connect Spotify and refresh the music library

Open **Music library** in the sidebar, or **Manage library** inside collection
search. The **Library refresh** and **Spotify connection** tabs use the daemon's existing
connection and refresh operations. They work before any speakers are discovered;
choosing a room is only needed when you play or queue music.

In **Spotify connection**, read the current status and review a new sign-in. Open the
validated Spotify consent link, allow read-only access to library and taste, then
return and check status. When using a phone or another computer, the completion
form accepts the full callback address from the Spotify tab and submits only the
required callback fields to this daemon. The form clears the pasted address
after submission or when closed or hidden. Sign-in status reports cached
credentials, not a live Spotify access check. Spotify must also be linked to
each Sonos household through its Sonos app.

Signing in does not automatically load music. Continue to **Library refresh** and
explicitly request a refresh. The panel distinguishes the last completed full
refresh from retained intake pages, artist genre enrichment and followed-artist
progress. It shows daemon-reported cooldowns, pending work, limits and partial
results without presenting retained pages as a complete public cache.
**Browse cached music** returns to collection search and reads the selected
category again; music selection keeps its existing playback review.

Status observation runs only for the visible tab while the panel is open.
Closing it, hiding the page or entering immersive mode stops those observations;
a refresh already accepted by the daemon continues there. Unconfirmed requests
are not replayed automatically. The design preview makes no connection or
refresh requests. Connecting and refreshing do not start or skip music.

## Play a Spotify link or audio stream

Use **Play a link** beside the stage's playback controls. Paste a Spotify share
link or URI, or the HTTP(S) address of an audio stream. Spotify tracking parameters
are removed when the address becomes its native URI; signed stream addresses are
sent with their original path, escaping and query intact. An optional display
title travels with the source.

**Play now** supports Spotify tracks, albums, playlists and episodes, plus audio
streams. Albums and playlists replace the current queue; individual sources
replace playback while leaving the existing queue available. Artist and show
addresses support **Queue next** and **Add to end**. These queue actions also
accept the other Spotify kinds, but never start or resume playback themselves.
Queue next appends when the group is not currently using its queue.

Review shows the exact canonical source, household-qualified destination, current
playback and effect. Confirmation rechecks group membership and relevant source,
queue revision and occurrence before one native command. Appending may proceed
after ordinary track progress; replacing playback or inserting next requires the
reviewed playback anchor. Container playback also checks the queue it will replace.
These browser observations supplement the daemon's own checks; they do not reserve
speakers against another controller.

Source actions use the same dashboard mutation and uncertainty lane as transport,
DJ and queue edits. They do not call a separate DJ-stop operation: a DJ session
can remain stored while a different source plays, and queue additions leave a
running DJ active. Native receipts show accepted ranges and policy notes. Lost or
malformed replies are not retried. Closing or hiding retires an unsent review;
once sent, the original destination and receipt remain available when reopened.
Typed sources survive refreshes and closing, but changing groups requires a new
review. Preview mode accepts drafts and never dispatches commands.

## Advanced direction for the current DJ

**Advanced direction** beside the mood presets reads the selected group's current
DJ and offers all native artist, genre, composer, period, decade, keyword, work
length and energy filters. Each name occupies its own line, so commas inside an
artist's name are preserved. The draft starts with the session's own additions;
it does not copy inherited mood filters into the replacement.

The session mood is separate from the effective mood shown in the observation.
An automatic session keeps following the house's time-of-day program, while a
named mood stays pinned for that session. The native status now includes
`session_mood`, explicitly null for automatic, alongside the unchanged effective
`mood`. An older response without that evidence requires a deliberate choice
before replacing an ambiguous session.

**Review direction** saves settings for future picks without starting or
restarting playback. When the DJ is stopped, **Review & start DJ** sends the
settings and start together so they apply from its first pick; a fresh DJ session
can replace the current source and queue. **Review return to program** clears
temporary steering. Already queued DJ works and standing preferences remain.

Keep the existing deadline, choose any new whole duration from 1 to 10,080
minutes, or keep the direction until cleared. Keeping a deadline uses the
daemon's remaining seconds read last during confirmation, not the duration first
shown or the browser's wall clock. A deadline with less than a minute remaining
needs an explicit new duration or a return to program.

Confirmation checks the exact qualified group, running state, steering, selected
mood definition, and, for a start, the current source and queue occurrence.
Changed observations retire the review. Reads expire after 45 seconds; edited
drafts survive an on-demand reread in the same group, while changing groups,
closing or hiding the dialog discards them. One native command uses the shared
dashboard lane, retains uncertain outcomes until explicit **Refresh**, and is
never retried automatically. There is no added DJ polling or hidden writer.

## Standing taste for every DJ

**Standing taste** in the Personal DJ panel edits the daemon’s persistent
preferences for every listening group. It works even when no speakers are online
or selected. Choose default energy from 0 to 100, return to the time-of-day
program, or allow/exclude explicit tracks. Favor or avoid genres, artists,
decades/classical eras and named moods; pin or ban artists, albums and tracks.
Each entry has its own removal control. Artist names containing commas remain
one value, and album/track pins and bans accept exact titles or Spotify URIs.

Favor increases selection weight. Avoid can relax if no other music fits; a ban
does not relax. Pins outweigh avoids. Temporary group steering takes precedence,
including an explicit request for otherwise banned music. These controls change
future DJ selections without starting, skipping or restarting playback, changing
queued music, or modifying the Spotify account. Preferences stay saved after the
browser closes. Speaker undo does not reverse them; edit or remove a value here.

Review shows one exact setting and its current value. Confirmation rereads that
setting before one native `POST /dj/preferences`; it never sends an old copy of
all preferences. Concurrent changes to other settings are retained. This is a
fresh check, not an atomic lock against other controllers. Reads are on demand,
expire after one minute, and stop when the dialog closes or the page is hidden.
The read-only preview shows fictional taste and cannot save.

Preferences use the dashboard’s shared command lane. An uncertain save blocks
further commands until **Refresh preferences** explicitly rereads current stored
taste. Refreshing playback or routines cannot acknowledge it, and preference
reads cannot clear uncertainty from those other commands. A submitted save is
never repeated automatically; its receipt remains available after closing or
hiding the panel.

## Rehearse a DJ set

Select a listening group and open **Rehearse a set** beside **Advanced direction**.
**Generate preview** calls the daemon's real planner through
`POST /dj/rehearse`, using its library, listening history, standing taste and
group steering. It shows what the DJ would choose without starting music,
changing the queue, saving steering or recording those hypothetical plays in
listening history. A stopped DJ can be rehearsed too.

The design preview lets you browse these settings, but generating a set requires
the running daemon and its actual library; it does not fabricate a rehearsal.

Choose 1–500 works or a window of 1–48 hours. The hour option includes works
whose starts fall inside that window, so a final whole work can finish later;
every preview is bounded to 500 works. Blank start means now. `HH:MM` selects
that time's next occurrence on the house's clock, including tomorrow when it
has already passed. An RFC 3339 date/time with an explicit UTC offset fixes an
exact start. Results keep the returned date and offset, including midnight
crossings, rather than converting them to the browser's timezone. Repeating a
seed reproduces the set only while the start time and other planning inputs
remain the same.

Leave the mood and temporary filters empty to follow the group's current
steering and time-of-day program as they change through the set. A named mood
or any nonempty temporary filter replaces that steering for this preview;
filters supplied with a mood are combined with its definition. Standing taste
continues to apply under the DJ's usual precedence rules. Temporary filters
include or exclude artists, genres, composers and keywords, with one literal
name per line. They also support work length bounds, an energy shift, long
works and strict selection. Strictness from either the mood or the checkbox
keeps filters in force when matching music is scarce; a preview can be empty.
These drafts do not change the group's live direction.

The result shows the whole-work sequence, start times, selection reasons,
relaxed filters and actual selection multipliers, plus energy targets and
explicit-content markers when reported. Hourly rows group works by their start
hour on the house clock. Their energy is the mean for those works; their target
is the first work's target. Artist and genre variety use Shannon entropy in
bits: zero means one artist or genre, and higher values mean more variety.
**Show more works** reveals another 24 already returned works without making a
new request.

A result is a snapshot, not a promise about future playback. Editing the form
labels the existing result with its captured settings until explicitly generated
again. Ordinary playback updates retain that snapshot; changing the selected
group or its membership retires it. New generation requires a fresh selected
group, and membership is checked before and after planning. Only one generation
can be in flight. Closing, hiding or entering immersive mode fences late
results, and there is no rehearsal polling or automatic retry. A failed planning
request does not make playback uncertain because it sends no playback command.

**Review settings for live DJ** carries a completed, unchanged rehearsal's
requested mood and temporary filters into **Advanced direction** as an unsaved
draft. The editor rereads the same group's current DJ and mood definitions,
then requires you to choose a lifetime and review **Save direction** or
**Start DJ with settings** before any write. A named mood uses its current
definition. The exact planned sequence, seed, start time, length and relaxed
selection filters are not transferred. Empty previews, edited settings and
previews that only inherited current steering have no transferable draft.
An existing unsaved direction draft or review is preserved until you close it.

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

### Save a queue as a Sonos playlist

Open **Speaker queue → Save queue as playlist** to keep the selected group's
retained queue on its household's speakers. The dialog reads the current group
and a bounded queue observation, shows the entry count, and asks for a title.
Titles are trimmed and may contain up to 1,023 UTF-8 bytes; control characters
are rejected. **Review new playlist** shows the exact title and listening group
before **Create Sonos playlist** sends one native save.

Every save creates a new playlist, including when the title already exists.
You can save a nonempty retained queue while radio or another source plays.
Saving leaves playback, queue order, volume and the DJ unchanged. Confirmation
rechecks group membership and the queue's count and revision, but these reads
do not lock the queue: Sonos copies its contents when the save command runs.

A confirmed receipt keeps the original group, title and native `SQ:` playlist
identity. If action logging fails, the playlist is still saved and the dialog
warns against repeating the save. **Browse saved playlists** reads the native
catalog and selects the confirmed identity in the existing source drawer;
playback still requires its separate review. An existing source draft is
preserved, and browsing a receipt requires selecting its original group.

An unconfirmed save blocks further writes until **Check saved playlists** reads
the catalog for the original qualified room, even if you have since selected a
different group. Playback refreshes do not acknowledge this storage outcome.
The returned catalog is an observation: matching titles cannot prove which
request created an entry, and a delayed save may still complete. Nothing is
retried automatically. Any deliberate new save needs another queue read and
review. Closing, hiding or entering immersive mode cancels unsent observations;
a submitted save retains its eventual receipt without replaying the request.

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

### Persistent routines

**Routines** beside the house scenes opens the daemon's saved schedule list.
Create a routine for a saved scene, including a complete queue backup, or for a
qualified listening group and DJ direction. The DJ editor shares the full
remote's mood, filter and relative-duration validation; choosing automatic
explicitly clears earlier temporary steering each time the routine runs.

Timing supports daily, weekday, weekend and custom-day recurrence, a relative
delay, or an exact date/time with seconds and an explicit UTC offset. Recurring
times use the **daemon host's local timezone**, including its daylight-saving
rules. The browser shows the daemon's reported next run and last fired/skipped
occurrence, with an explicitly labelled browser-time conversion. It never
calculates a substitute next run.

Every creation shows its complete reviewed target, timing and DJ direction or
current scene contents. Confirmation rereads the stored schedules, checks for an
equivalent routine, and verifies the scene, group and named mood before one
native write. Each future scene run resolves the saved name at execution time;
replacing or deleting that scene changes what future runs can restore.

Pause, resume and removal review the exact stored ID and definition. They do not
stop an occurrence already in progress, and resuming does not replay missed
occurrences. Routines persist after closing the browser and execute with their
creator's rights under current house policy. The daemon skips runs more than ten
minutes late.

Routine storage remains manageable with no selected or online speakers. It
shares the same dashboard command lane as playback, with no automatic retry.
An uncertain schedule write requires explicit **Refresh routines** and inspection
of the returned list; refreshing playback does not acknowledge it. Reading
routines cannot clear an uncertain speaker command. Closed or hidden panels,
expired reviews and changed definitions prevent unsent actions. Preflight is a
fresh check, not a server-side lock against another client.

### Announcements

**Announcements** beside the house scenes supports spoken messages, built-in
bell/beep/rise chimes, and a local WAV upload through the native announcement
service. Speech is generated on the daemon host, with an optional installed voice
name. The browser does not synthesize speech or read a daemon filesystem path.

Read the physical rooms and select each target explicitly. Same-name rooms in
different households remain separate, qualified choices; no rooms are selected
automatically. Review the message or clip, selected rooms, affected groups and
requested volume before sending. The daemon applies room limits, handles
temporary grouping and playback, then attempts to restore the original state.

WAVs are checked locally before review: complete RIFF/WAVE, mono or stereo 16-bit
PCM, one of the daemon's supported sample rates, at most 16 MiB and five minutes.
The daemon independently validates uploaded bytes. Replacement, source changes,
closing or hiding the panel cancel a pending local read and discard its bytes.
The file is uploaded only after confirmation.

Announcements share the dashboard's single command lane with playback and
routines. Confirmation rereads the house groups; changed groups, stale reviews,
closed dialogs and hidden pages prevent an unsent action. Once submitted, closing
the dialog does not cancel or repeat the announcement. The bounded native request
allows up to 20 minutes for synthesis, waiting, playback and restoration.
A missing or malformed reply leaves the command uncertain; inspect playback and
use explicit Refresh before another action.

The last received result remains available when reopening the dialog. It shows
each native household report, actual applied levels, playback outcome and every
reported restoration failure. A valid partial result stays visibly incomplete;
an incomplete room receipt cannot claim successful restoration. Native results
omit household IDs, so repeated room names are retained without inventing a
unique household correspondence. A current, active dashboard view refreshes
playback after an acknowledged announcement; another room read is required
before a new review.

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
node --test scripts/test-dashboard-direction.cjs scripts/test-remote-dj-steering.cjs
node --test scripts/test-dashboard-source.cjs scripts/test-remote-source-player.cjs
node --test scripts/test-dashboard-playlist-save.cjs scripts/test-dashboard-playlist-save-panel.cjs
node --test scripts/test-dashboard-preferences.cjs scripts/test-remote-dj-preferences.cjs
node --test scripts/test-dashboard-rehearsal.cjs scripts/test-remote-dj-rehearsal.cjs
python3 scripts/test-dashboard-browser.py --browser-executable /path/to/chromium
python3 scripts/test-dashboard-preferences-browser.py --browser-executable /path/to/chromium
python3 scripts/test-dashboard-rehearsal-browser.py --browser-executable /path/to/chromium
python3 scripts/test-dashboard-workspace.py --browser-executable /path/to/chromium
python3 scripts/test-dashboard-preview.py
```

The browser suites require Python Playwright and Chromium. They exercise the
production assets with explicitly synthetic API responses, not physical speakers.
The rehearsal browser suite accepts `--screenshots /path/to/output`; the other
browser suites use `--screenshots-dir /path/to/output` to capture the interface.
The Node suite covers native payloads, immutable reviews, real loopback HTTP
session dispatch, queue positions, stale/hidden lifetimes and keyboard navigation.
Rehearsal cases cover native planning requests, captured filters, midnight
offsets, whole-work reasons, group changes, retained snapshots during playback
updates and late-result handling. The browser rehearsal fixture exercises the
actual form and result rendering at desktop and mobile sizes, including mobile
dark and light themes.
The preview test serves the actual bundle through a real loopback HTTP listener.

These client checks do not establish Rust workspace compilation, real daemon/GENA
integration, physical-speaker operation, or microphone/fullscreen permission on a
user's browser. Those require their own runtime or hardware checks.
