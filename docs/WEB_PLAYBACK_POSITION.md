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
