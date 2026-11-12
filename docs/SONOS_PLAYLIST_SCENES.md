# Saved Sonos playlists in scenes and routines

A scene can group rooms, set their levels, and load a playlist saved in that
Sonos household. The same scene can run on a schedule. Both S1 and S2 use their
own saved catalogs; no Spotify Web API sign-in is needed to select a native
playlist. The speakers still need access to the music services or files it
contains.

## Create and apply a scene

Read the destination household's saved catalog and choose its native `SQ:` id:

```sh
fsonos playlists Kitchen
fsonos playlist-contents Kitchen SQ:7
```

Write a scene such as `morning.toml`:

```toml
[[groups]]
coordinator = "Kitchen"
members = ["Office"]
source = { kind = "sonos_playlist", name = "SQ:7" }
playing = true

[volumes]
Kitchen = 20
Office = 15
```

Then save, inspect and apply it:

```sh
fsonos scene save morning --from morning.toml
fsonos scene show morning
fsonos scene apply morning
fsonos schedule add "weekdays 07:30" scene apply morning
```

Saving only reads the house and catalog and writes the local scene. Applying
it replaces the destination group's current queue with the saved playlist's
**current contents**, in its saved order, including repeated tracks. Each recall
reloads those contents; it does not treat an already selected queue as proof
that nothing needs to change.

Set `playing = false` to load the playlist ready without starting it. This also
silences an old source that was playing; the scene never starts the new queue
just to pause it afterward. If `playing` is omitted, the scene retains whether
its destination was playing when application was planned.

An authored native playlist source needs an exact `SQ:<digits>` id. Unlike
immediate whole-playlist commands, it does not accept a title, title prefix or
catalog position. Use `Kitchen@S1` or `Kitchen@S2` where room names repeat. All
members of one scene group must be in that coordinator's household.

## Use the phone remote or dashboard

In **Scenes**, create a custom scene, choose its destination group, then choose
**Saved Sonos playlist** as the source. The picker reads that household's
catalog and retains distinct native ids even when titles are identical. Review
the selected playlist, rooms and playback state before saving.

The review checks the catalog and destination again before sending the save.
Changed room identity, a removed selection or a renamed title requires a fresh
review. The returned scene must match the reviewed target and playlist; a lost
or inconsistent receipt is not permission to repeat the save automatically.
Open **Scheduled routines** on the phone remote, or **Routines** on the
dashboard, and choose a new **Apply scene** routine. Select the saved scene,
choose its days and time, then review and create it. The chooser accepts native
playlist scenes alongside DJ, stream and captured-queue scenes. Its review
retains the native household, id, title and playback intent; a changed saved
binding requires a new review before the routine can be created. Scheduling
reads local scene definitions and does not start music or require an online
speaker. Catalog readiness is checked when the routine runs.

## HTTP and MCP

For HTTP, send the scene definition to `PUT /scenes/morning`:

```json
{
  "groups": [{
    "coordinator": "Kitchen",
    "members": ["Office"],
    "source": {"kind": "sonos_playlist", "name": "SQ:7"},
    "playing": true
  }],
  "volumes": {"Kitchen": 20, "Office": 15}
}
```

For MCP, call `save_scene` with `name: "morning"` and the same definition as
its `scene` argument. Apply with `POST /scenes/morning/apply` (an empty JSON object)
or MCP `apply_scene`, naming `morning`. A schedule uses `POST /schedules` or MCP `add_schedule` with
`{"when":"weekdays 07:30","action":"apply_scene","scene":"morning"}`.

The saved source adds a read-only `playlist` object containing `household`,
`id` and `title`; `name` remains the native id. Clients must not author this
binding or provide a renderer URI. The server resolves it from the selected
room's catalog. Ordinary scene capture still captures the active queue using
the existing [queue bookmark or backup rules](SCENE_QUEUES.md); use an authored
source to recall a saved Sonos playlist by identity.

## Identity, permissions and failure behavior

Saving pins the observed household, native id and exact title. Recall refuses
a missing playlist, a renamed title, or a target moved to another household;
the same `SQ:` id in another household cannot become its replacement. Save the
scene again to accept a deliberate rename. Editing the playlist's contents
while its id and title remain the same is supported: future recalls use those
new contents.

Native sources use the existing `play_favorite` permission and `playback`
policy class. A silent load also requires `pause`. Both the caller and the
scene saver must permit each operation, and their applicable playback volume
caps still apply. Catalog readiness is checked only for permitted sources,
before cap lowering or grouping writes. Source dispatch resolves the catalog
again and checks the scene's confirmed audience before loading it.

A scheduled scene waits for catalog and target readiness before claiming its
occurrence, within the scheduler's usual ten-minute grace. Once execution has
started, a partial or unacknowledged write consumes that occurrence and is not
replayed. Scene results report which operations failed. A read failure is never
treated as an empty playlist, and a failed append is never blindly repeated.

Loading uses the existing staged container replacement: it verifies the new
insertion before removing the previous queue prefix. This is not an atomic
device transaction. A failed recall can leave the old source paused, newly
appended tracks, or a partly completed scene. Concurrent external changes can
still occur between observations and commands. Inspect the current house and
queue before deciding to apply again.

Replacing a queue is **not recoverable through action-history undo**. Undo can
attempt the prior transport and room settings but cannot reconstruct the
replaced queue contents. Save a native playlist or a durable queue backup
beforehand when those contents must be retained.
