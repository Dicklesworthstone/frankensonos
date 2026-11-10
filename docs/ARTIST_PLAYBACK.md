# Play a Spotify artist

An artist link selects up to ten playable tracks in Spotify's order. It uses
the artist's top tracks when available. If Spotify refuses that endpoint with
403 or 404, it reads a bounded sample from the artist's albums and singles.
The response identifies which selection was used. This is a finite selection,
not artist radio, a complete discography, or a popularity ranking of the fallback.

## Use it

Complete `fsonos setup` for the owner's Spotify app and sign-in. The destination
household must also have its Spotify account linked in Sonos and a Spotify
favorite from which FrankenSonos can learn the track-rendering parameters.
Web sign-in reads metadata; the speakers play through their existing Sonos link.

```sh
fsonos play Kitchen 'https://open.spotify.com/artist/<22-character-id>'
fsonos play Kitchen 'spotify:artist:<22-character-id>' --next
fsonos play Kitchen 'spotify:artist:<22-character-id>' --queue
```

`--direct` works with the same saved Spotify configuration. The running daemon
and standalone MCP read newly saved app settings and credentials on the next
artist request, including setup completed after startup.

| Action | Result |
| --- | --- |
| Play now / `play` | Replace the queue with the complete selected track list and start track one in `NORMAL` order. Apply the caller's volume caps to the actual group first. |
| Queue next / `--next` | Insert the tracks in selection order after the current queue occurrence. When the group is not using a nonempty queue, append instead. |
| Queue at end / `--queue` | Append the ordered selection. |

Queue actions do not start, resume, pause, change play mode, or change volume.
They retain a running DJ. A later queue undo removes only the exact added range.
Replacing a queue cannot recover its former contents through ordinary action
undo, and the action history marks that boundary.

HTTP uses the existing `POST /play` and `POST /queue` bodies with an artist
`source_uri`. MCP uses `play` and `add_to_queue`. The phone and listening-room
dashboard offer the same three actions, show the selection bound and sign-in
requirement, and recheck the destination and queue before sending once.

## Selection and failure behavior

The resolver keeps distinct tracks credited to the requested artist, skips
local or explicitly unplayable items, and rejects malformed or inconsistent
track identities. It returns a complete valid selection or an error. Empty top
tracks are an error; only the explicit 403/404 response permits catalog fallback.
The fallback examines the first ten album/single entries, reads the first track
page of at most three distinct credited albums, and stops at ten tracks. It
does not follow catalog `next` links.

This is an explicit playback request, so DJ taste filters and implicit feedback
do not choose or censor its tracks. Explicit recordings can be included. A
classical selection can contain individual movements; use the whole-work DJ
when a complete work is required. The first selected track enters listening
history only when it is actually observed playing. Queue additions are not
treated as listens.

The destination source, group membership, boot epoch and original queue are
retained across catalog lookup and checked before writes. Current policy and
renderer availability are checked again after lookup. An owner changing the
queue or source while Spotify responds causes refusal rather than adopting
that newer state for replacement. The resolver has a 45-second total deadline,
admits one lookup at a time, and respects Spotify's cooldown across requests.
It does not take the DJ's execution lock or change the cached music library.

Sonos has no atomic multi-track transaction. Each add is sent once, read back,
and checked against the complete queue. The original queue remains until every
new track is verified; native revision checks protect removal of its old prefix.
A partial failure can leave some new tracks appended or inserted. A failure
after prefix removal can leave the new queue stopped. Such outcomes are reported
as uncertain and are never automatically replayed. Read the current queue before
making another deliberate request. These checks cannot reserve a speaker against
all subsequent actions from another controller.

## Scope and validation

Bare artist IDs still do not start reliably through Sonos' native artist
renderer. This feature sends concrete track URIs instead. Show links remain
queue-only. Authored scenes do not resolve artist or show sources; use a concrete
track, album, playlist or DJ scene.

Regression coverage is in `artist_playback`, `spotify_track_batch_sim`,
`spotify_artist_sim`, the CLI `spotify_sources` tests and
`e2e_artist_playback`, plus the phone/dashboard source suites. The three affected
browser suites passed 122 tests in this session. Rust regressions were added but
could not run locally: compiling the pinned `asupersync` dependency was killed
before project type checking. GitHub CI is the remaining execution check.
No physical-speaker or real-account acceptance claim is made by these fixtures.
