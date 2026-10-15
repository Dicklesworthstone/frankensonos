# Play a link or stream from the web remote

Open **Play a link or stream**, select the destination group, and paste a
Spotify track, album or playlist share link (or its `spotify:` URI). The item
does not have to be in the synced library. An optional title supplies display
metadata. Select **Review playback**, inspect the group and its current
source, then **Play now**.

Spotify locale, embed and legacy playlist links are normalized to their
canonical Spotify URI; share tracking parameters are discarded. Shortened
`spotify.link` URLs require opening the link yourself and pasting its full
address. Artist, show and episode playback is not exposed by this control.
Spotify still requires the household's existing Sonos account linkage and
learned rendering parameters; this interface does not bypass them.

Direct HTTP and HTTPS audio addresses also work. The browser does not fetch
or preview the pasted address: it sends it to the existing `/play` API, and
the speakers fetch the audio. Supply an actual audio stream, not a player web
page. The speaker must support its format and be able to reach the source.
Embedded sign-in details, local-file and executable URI schemes are refused.
Source addresses and optional titles pass through the ordinary action log;
do not paste private access-bearing addresses into a shared controller.

Playing an album or playlist replaces the group's queue. Playing a track or
stream replaces its current source. The review says which effect applies.
The existing policy, volume caps, playback orchestration and action history
remain authoritative. This is the native `/play` route, not a Spotify Web
API playback command or a new privileged endpoint.

A review is consumed when edited, canceled, closed or confirmed. Confirmation
rechecks exact group membership and the current source/queue position before
sending once. It shares the room cards' native action lock. These client
checks are not an atomic reservation against other controllers or the
speakers' own transitions. Lost or malformed replies remain explicitly
uncertain, are never replayed, and require checking the room before another
deliberate action. A successful response acknowledges the command, not proof
that the remote audio service is audible.

Validation: `node --test scripts/test-remote-source-player.cjs` executes the
shipped parsing, controller, DOM panel and mounting adapter. It also checks
a real loopback HTTP exchange. Physical Sonos/Spotify acceptance is a separate
hardware check; the loopback server does not emulate the Rust daemon.
