# Playback position in the web remote

Every room-group card shows elapsed time, duration and remaining time when
reported by the daemon. While playing, the `≈` marks local interpolation from
the most recent sample. Paused, stopped, transitioning and unknown states do
not advance. Reaching an estimated end does not invent a next track or stop.

Unknown position or duration stays unknown; a live stream with no duration
can show elapsed time without an invented progress percentage. The **Refresh
position** button reads that exact household-qualified group's state. It does
not start playback, discover another destination or read Spotify.

The clocks share the remote's ordered state reads and event-reconciliation
path. They do not poll once per second. Lost event connections, state errors
and topology invalidations freeze the last sample until a fresh read arrives.
Hidden pages stop their clocks; returning waits for the shared refresh.
Regrouping retires the affected card, its clock and its event listeners.

Run `node --test scripts/test-remote-playback.cjs` for the controller, actual
card-lifecycle integration and real loopback HTTP tests. These tests do not
establish physical-Sonos acceptance.

## Seek within the current track

Use the position slider, **Restart**, **−15 seconds** or **+15 seconds** to
preview a position, then press **Seek to …**. Moving the slider never sends a
command by itself. Cancel, refresh, track changes, group changes and hiding
the page discard an unsent selection. Natural elapsed time and title-only
updates do not move a chosen slider position.

Seeking is available for a playing or paused track with a reported position
and positive duration. Unknown-duration streams and unreadable states remain
read-only. The final selectable position is the last whole second before the
end; no preview silently skips to another track. A reported duration does not
guarantee that a renderer supports seeking: its error and remedy are retained.

Confirmation shares the card's ordinary action lock with play, next, previous
and DJ controls. It first re-reads the exact household-qualified group's state
and checks membership, track URI, queue position (including repeated tracks),
duration and transport state. It sends the existing `POST /seek` body with only
`zone` and absolute `to_secs`. Seeking paused music does not issue a resume,
source replacement, volume change or queue edit.

The preflight is a client-side check, **not an atomic track reservation**: the
current API has no conditional-seek field, and another controller can change
the speaker between the read and the seek. No stronger guarantee is implied.

A successful reply is shown as accepted, not as proof that the slider reached
the requested position; the clock uses the subsequent observed state. A lost,
malformed or rejected seek reply is never automatically repeated. Passive
state updates can show where playback is now, but an uncertain seek keeps its
controls disabled until **Refresh position** succeeds. The old confirmation
stays consumed even then: another seek requires a new preview and confirmation.
