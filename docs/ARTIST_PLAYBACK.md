# Play Spotify artists and shows

An artist link selects up to ten playable tracks in Spotify's order. It uses
the artist's top tracks when available. If Spotify refuses that endpoint with
403 or 404, it reads a bounded sample from the artist's albums and singles.
The response identifies which selection was used. This is a finite selection,
not artist radio, a complete discography, or a popularity ranking of the fallback.

A show link selects up to ten distinct, available episodes from its first
episode page, in Spotify's returned order. It does not promise newest or
unplayed ordering. Episodes remain separate from the music library and the
DJ's selection pool.

## Use it

Complete `fsonos setup` for the owner's Spotify app and sign-in. The destination
household must also have its Spotify account linked in Sonos and a Spotify
favorite from which FrankenSonos can learn the track-rendering parameters.
Web sign-in reads metadata; the speakers play through their existing Sonos link.

```sh
fsonos play Kitchen 'https://open.spotify.com/artist/<22-character-id>'
fsonos play Kitchen 'spotify:artist:<22-character-id>' --next
fsonos play Kitchen 'spotify:artist:<22-character-id>' --queue
fsonos play Kitchen 'https://open.spotify.com/show/<22-character-id>'
fsonos play Kitchen 'spotify:show:<22-character-id>' --next
fsonos play Kitchen 'spotify:show:<22-character-id>' --queue
```

`--direct` works with the same saved Spotify configuration. The running daemon
and standalone MCP read newly saved app settings and credentials on the next
artist or show request, including setup completed after startup.

| Action | Result |
| --- | --- |
| Play now / `play` | Replace the queue with the complete selected track list and start track one in `NORMAL` order. Apply the caller's volume caps to the actual group first. |
| Queue next / `--next` | Insert the tracks in selection order after the current queue occurrence. When the group is not using a nonempty queue, append instead. |
| Queue at end / `--queue` | Append the ordered selection. |

Queue actions do not start, resume, pause, change play mode, or change volume.
They retain a running DJ. A later queue undo removes only the exact added range.
Replacing a queue cannot recover its former contents through ordinary action
undo, and the action history marks that boundary.

HTTP uses the existing `POST /play` and `POST /queue` bodies with an artist or
show `source_uri`. MCP uses `play` and `add_to_queue`. The phone and listening-room
dashboard offer the same three actions, show the selection bound and sign-in
requirement, and recheck the destination and queue before sending once.

## Artist selection and failure behavior

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

## Show selection

The show resolver reads one show document using the owner's existing Spotify
session and account country. It verifies the requested show identity, accepts
at most the first 50 returned episode entries, and keeps up to ten distinct
playable, unrestricted episodes in provider order. It never follows a returned
`next` URL. Missing sign-in, an empty selection, malformed identities or Spotify
refusal returns an error before speaker changes. No extra OAuth scopes are
requested. Explicit episodes can be included.

Show playback and queue additions use the same guarded batch operations as
artists. Episode URIs and metadata use the household's learned Spotify service
id with the existing episode renderer contract. The shared catalog worker
retains rate-limit cooldowns across artist and show requests.

## Scenes and scheduled routines

An authored scene can use either catalog URI as a source. Saving records the
URI without contacting Spotify; each recall resolves a fresh bounded selection.
For example, the source in a scene definition can be:

```json
{
  "groups": [{
    "coordinator": "Kitchen",
    "members": [],
    "source": {
      "kind": "uri",
      "uri": "spotify:show:<22-character-id>"
    },
    "playing": false
  }],
  "volumes": { "Kitchen": 15 }
}
```

A paused scene selects the episode queue without sending Play. Setting
`playing` to true starts the first selected episode. Artist sources use the
same structure with `spotify:artist:`. The selected items can change between
recalls because the scene retains the catalog request, not a fixed episode or
track snapshot.

All permitted source lookups and destination renderer checks finish before
the scene changes volumes, fades or groups. Repeated sources within a readiness
phase resolve once across households. The saved writer's and current caller's
permissions and caps are checked again after lookup. Grouping uses each
destination household's own renderer, and queue replacement creates an undo
barrier. Independent speaker changes are checked before the selection is
consumed; an uncertain scene is not automatically replayed.

Scheduled scene recall uses the same source readiness checks. Missing catalog
authorization or unavailable results leave the scheduled occurrence unclaimed
for a later attempt. The scheduler checks readiness again when executing;
each phase can read a fresh selection. Paused scheduled scenes remain silent.

## Scope and validation

Bare artist and show IDs do not start reliably through Sonos' native container
renderers. These workflows send concrete music tracks or episodes instead.

Regression coverage is in `artist_playback`, `show_playback`,
`spotify_track_batch_sim`, `spotify_artist_sim`, `spotify_show_sim`, the CLI
`spotify_sources` tests, `scenes_catalog_sim` and both catalog playback e2e files, plus the
phone/dashboard source suites. Rust execution of the new catalog workflows
remains pending in GitHub CI: the local dependency build exceeded available
resources, then the execution environment became unavailable. The earlier
browser suites passed 122 tests before show support was added; expanded Node
suites still need execution. Eleven focused controller checks for the updated
artist/show modes, queue freshness and show descriptions passed in V8.
Physical-speaker and real-account acceptance remain separate checks.
