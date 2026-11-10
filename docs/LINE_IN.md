# Play a speaker's native line-in

FrankenSonos can discover physical line-in inputs on your Sonos household and
play one through a room's current group. A turntable or other audio source can
remain connected to one speaker while another group in the same household
listens. The source player and the destination coordinator are independent.

The controller reads each discovered player's own device description and
AudioIn attributes. It does not infer support from a model-name list or require
Spotify authentication. S1 and S2 households remain separate.

## CLI

```sh
# Read inputs in the household containing Study. This changes no playback.
fsonos line-in Study
fsonos --json line-in Study

# Play the input belonging to a source room on Study's current group.
fsonos play Study --line-in "Music Room"

# Use the exact player id returned by discovery to select a particular jack.
fsonos play Study --line-in RINCON_SYNTHETIC01400
```

Use qualified room names such as `Study@S1` when households have duplicate
names. A room or single-room alias selects its primary player. An exact id
from discovery selects that physical player's input, including the secondary
player of a stereo pair; it is not silently replaced with the primary's id.

The ordinary daemon selection applies: a running daemon, including one reached
through Tailscale, handles the request; `--direct` talks to the LAN through the
same shared controller. A dispatched daemon request never falls back to another
local speaker after an uncertain response.

Line-in is a live source. `--next` and `--queue` are rejected, as are a second
source, a favorite, a saved playlist or a search in the same play request.
Start the external source device yourself after selecting its input.

## Phone browser and dashboard

In the source selector, choose **Line-in**, choose the destination, then
**Read line-in sources**. Select an input and review **Play now**. The review
shows the source room and destination group. Sending it rereads the selected
input and destination; a removed or changed selection requires a new review.

Changing the destination retires its previous input catalog. Selecting an input
never starts playback by itself, and an uncertain write is retained for
inspection rather than automatically sent again.

## Native Apple app

Open **Music → Line-in** to read inputs in the selected S1 or S2 household.
Each source shows its name, room and exact player UUID, so same-named inputs
and both jacks of a stereo pair remain distinguishable. Players that could
not be read appear separately; they do not hide the healthy inputs.

Choose **Play now** and use the existing command review sheet. Preparation and
confirmation reread the selected input's identity and metadata before the
fresh destination observation. A changed source or group requires a new
review. Changing households, leaving the screen or cancelling a read cannot
revive an old selection, and a lost acknowledgement cannot replay a consumed
command. Line-in offers no queue action.

## HTTP and MCP

`GET /line-in?zone=Study` returns an object with two arrays:

```json
{
  "sources": [
    {
      "player": "RINCON_SYNTHETIC01400",
      "room": "Music Room",
      "name": "Turntable",
      "icon": "Phono"
    }
  ],
  "unavailable": []
}
```

Only sources in the destination's household are listed. A successfully read
player without an AudioIn service is omitted. A player whose description or
input attributes cannot be read appears separately in `unavailable`, with its
player id, room and detail; healthy inputs remain selectable. An empty
`sources` array with unavailable players is an incomplete observation, not
evidence that the household has no inputs.

`POST /line-in/play` accepts only a destination and source selector:

```json
{
  "zone": "Study",
  "source": "RINCON_SYNTHETIC01400"
}
```

It returns the standard `OutcomeDto`, including any volume-cap notes. The
caller supplies no renderer URI or metadata. MCP exposes the same operations
as `list_line_in(zone)` and `play_line_in(zone, source)`, including structured
results and the same error contract. Reading uses the existing `list_players`
permission; playback uses `play`.

## Playback and recovery

Playback selects `x-rincon-stream:<source-player>` on the destination's current
coordinator, then starts it. The destination keeps its queue entries; the action
snapshot retains its prior source, queue position and playback settings for undo.
It does not regroup rooms. A source room outside the destination group keeps
its playback and volume. Existing caps and quiet hours apply to every room
that will sound in the destination group, including the input's room when it
is a member; the CLI retains its configured policy behavior.

Preparation checks the source's current advertised capability and identity,
same-household membership, destination source and audience, and available boot
identities. Those observations are checked after action snapshots and at both
playback-write boundaries. A stale request is not redirected onto a newly
elected coordinator or a same-named replacement input.

The action is recorded with the normal playback snapshot. `fsonos undo` can
restore the prior source and queue position through the existing ownership and
policy checks. A failed source lookup causes no volume or playback writes.
When a write's reply is lost, the existing direct-source readback checks may
confirm its effect; the write itself is not repeated. An unconfirmed or
partially applied request reports uncertainty and leaves the current state
available for inspection.

These are read-before-write checks, not a transaction across independent
Sonos controllers. An external command can still arrive between a read and a
write. The result reports a selected source and accepted playback command,
not a measurement that sound reached the listener.

## Connection state and validation

AudioIn's `GetAudioInputAttributes` reports the input name and icon. It does
not report whether the cable is connected or a signal is present. The current
discovery response therefore has no invented `connected` value. This workflow
does not configure input gain, autoplay, TV input or device firmware.

Coverage includes protocol argument/parsing tests, S1/S2 simulator discovery
and playback, exact stereo-pair source identity, household isolation, retained
queues, caps, undo, changed sources, interrupted replies, CLI direct/daemon
routing, HTTP validation, MCP forwarding, phone/dashboard review state, and
native Apple catalog, command and screen lifecycle contracts.
The simulator uses real localhost sockets. Physical line-in audio still needs
an opt-in check on the owner's speakers; simulator behavior does not establish
analog audio quality or hardware acceptance.

Protocol references: [AudioIn device-service contract](https://sonos.svrooij.io/services/audio-in),
[SoCo's native source selection](https://github.com/SoCo/SoCo/blob/master/soco/core.py),
and Sonos' [same-household line-in grouping contract](https://docs.sonos.com/reference/playback-loadlinein-groupid).
