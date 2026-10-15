# Listening Room

A local-first graphical dashboard and immersive visualizer, served by the same
FrankenSonos daemon as the full remote. No frontend build, package installation,
CDN, remote font, analytics, or cloud account is required.

## Open it

Rebuild FrankenSonos after updating the checkout, then start the daemon:

```sh
cargo run -p fsonos-cli -- serve
```

Open the HTTP address printed by the daemon in a local browser and choose
**Listening room & visualizer**, or append `/dashboard` to that address. The
existing full remote stays at `/`. The dashboard also uses the same listener
and origin rules when reached through Tailscale Serve.

To try the visual design without Rust or any speakers:

```sh
python3 scripts/dashboard-preview.py
```

Open `http://127.0.0.1:8765/dashboard?demo=1`. The preview server binds loopback
only and has no control API. The dashboard's explicit preview mode uses synthetic
rooms and music, displays a persistent preview banner, and cannot issue speaker
commands. `/dashboard?demo=1` works on the daemon too. Preview music is fictional,
not a claim about the owner's library or currently playing speakers.

## The dashboard

- **Now playing:** the selected group's artwork, title, artist, album, observed
  transport state, estimated elapsed time and group volume. Unknown information
  remains unknown. Artwork goes through the existing same-origin `/art` endpoint;
  locally generated cover graphics are a fallback, not claimed album artwork.
- **Your rooms:** select independent groups across households. Each card names
  the coordinator, other members, playback state and any stale observation.
  The sound map shows the actual grouping; highlighted nodes are coordinators.
- **Control:** pause/resume, next, previous and group-volume steps of five use
  the native public API, including existing policy, logging and undo behavior.
  No source replacements, queue reconstructions, or hidden commands are added.
- **Context:** the selected DJ's explanation, mood, next pick and reported
  problems, plus the four latest recorded plays. Advanced DJ, queue, sound,
  grouping, scenes and scheduling controls remain in the linked full remote.

## Visualization

**Aurora** draws flowing light ribbons, **Orbit** draws layered circular fields,
and **Contour** draws evolving topographic lines. These are procedural ambient
visuals, not fake speaker measurements, BPM estimates, or Sonos frequency data.
They respond to observed playback state and the selected track's identity.

**Immersive** expands the listening stage and requests browser fullscreen from
that click. When native fullscreen is unavailable or denied, the full-window
layout still works. **Back to dashboard** or Escape returns to the previous
view and focus. Keyboard focus stays within the immersive controls.

The visualizer respects the operating system's reduced-motion preference;
**Resume motion** explicitly enables movement. Animation is capped at 24 frames
per second and approximately 1.8 million backing pixels. Hidden pages stop
animation and event subscriptions. Canvas failure leaves the dashboard controls
usable without animation.

### Real, optional audio reaction

**Use microphone** asks for the browser's microphone permission. This analyzes
this computer's default audio input with Web Audio, entirely in browser memory.
It is not a direct feed from Sonos, and anything audible to that microphone can
influence the visuals. No recording, upload, persistent audio storage, or audio
output connection is created.

The interface explicitly distinguishes **Live microphone** from **Ambient**.
**Stop microphone**, a disconnected input, page hiding, and leaving the page
release the tracks and audio context. Capture never automatically resumes.
Cancelling or timing out a pending permission also fences late permission grants
so they cannot leave an input running. Permission requests time out after 30 s.

Microphone access requires a compatible browser on localhost or HTTPS. A direct
non-loopback HTTP listener may not qualify; ambient mode still works there.
Fullscreen, microphone permission, and the availability of input hardware remain
browser/OS capabilities, not guarantees of the dashboard.

## Reliability boundaries

One SSE connection coalesces changes into refreshed observations. A bounded
15-second read fallback catches missing events, backing off on failures. Playback
reads use at most four concurrent requests; DJ and history reads are separate.
Reads have a ten-second connection/body deadline. Hidden pages stop scheduling
reads, close SSE, and invalidate results that arrive after suspension.

Before a control command, the dashboard re-reads the selected group and checks
membership and, for transport commands, the track and playback state. This is
read-before-write, not an atomic physical-speaker reservation: another controller
can still intervene between the check and the command.

Commands are sent once and never automatically replayed. A missing, malformed,
or failed command response is surfaced as uncertain, even when a later passive
read shows a new state. Use **Refresh** to inspect playback and re-enable controls.
A response acknowledging a command is distinct from subsequently observed state.
Group-volume policy clamps and other native response notes are shown verbatim.

## Validation

```sh
node --test scripts/test-dashboard.cjs scripts/test-dashboard-audio.cjs scripts/test-dashboard-http.cjs
python3 scripts/test-dashboard-browser.py
```

The browser suite requires Python Playwright and Chromium. An installed Chromium
can be selected with `--browser-executable /path/to/chromium`. It renders the real
assets with explicitly synthetic API fixtures; it does not contact real speakers.
The Node suite includes real loopback HTTP tests as well as deterministic model,
permission-lifecycle, animation-budget and asset-embedding regressions.

Implementation-session checks: 42 Node tests passed, plus Chromium component
checks for native control payloads, household targeting, uncertain outcomes,
event refresh, offline recovery, safe text rendering, hidden-page behavior,
preview isolation, all three modes, reduced motion, immersive focus/Escape,
microphone fallback, and 1440/768/390/320-pixel viewports without overflow.
The environment's browser policy denied native fullscreen and real microphone
capture, so CSS immersion and microphone fallback were exercised there; actual
analysis data/lifecycles were tested with deterministic audio fixtures. No Rust
compiler or physical Sonos hardware was available: Rust workspace checks, real
GENA/daemon integration, and physical-device acceptance remain separate gates.
