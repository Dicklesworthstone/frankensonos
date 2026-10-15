# Queue native Sonos favorites

`POST /queue` and the MCP `add_to_queue` tool accept a native favorite as an
alternative to a Spotify URI. This queues a track, album or playlist without
clearing the existing queue, starting playback, changing shuffle/repeat, or
switching away from a station. The group coordinator is resolved from fresh
topology, including when the selected room is a group member.

Read `GET /favorites?zone=Den@S2` (MCP `list_favorites`) first, then send:

```json
{"zone":"Den@S2","favorite":"FV:2/1","next":true}
```

Use `next:false` or omit it to append. `next:true` inserts after the current
queue track; when another source is active it appends without changing that
source. A favorite can also be named by its numbered position or unambiguous
title. IDs come from the selected household's own list, not a combined library.
`Room@S1` / `Room@S2` disambiguates repeated room names.

Give exactly one of `favorite` and `source_uri`. Existing Spotify bodies and
the Rust `QueueAddRequest` type remain compatible. `title` is only valid with
`source_uri`: native favorites keep their original URI and DIDL metadata. The
API never accepts replacement favorite metadata from a caller. Stations and
browse shortcuts are not queueable; play a station with `play_favorite`.

Native favorites use the household's existing service linkage. They do not
require Spotify Web API library authorization or a separately learned Spotify
track render template. The speaker must still be able to play the service.

## Confirmation, policy and undo

Both source kinds use the `add_to_queue` permission. Native lookup failures,
rejections and uncertain writes are logged; only a confirmed insertion records
its exact queue identities for undo. MCP additions prefer the running daemon so
its policy, inventory and action log remain authoritative. An error from that
daemon never triggers a second local add.

The native add is sent once. Read-back must show the expected insertion with
the original neighbors unchanged. A lost single-track reply may be recovered
when the exact track is found; a lost container reply remains uncertain because
its service expansion cannot be attributed without the acknowledgement. Read
the queue before another add. External controllers can still race these reads;
this is not a device-side lock or a cross-client transaction.

Regression commands (Rust/simulator tests; physical speakers are not required):

```text
cargo test -p fsonos-core favorite_queue
cargo test -p fsonos-core --test favorite_queue_sim
cargo test -p fsonos-api surface::queue::input
cargo test -p fsonos-api --test favorite_queue_sim
cargo test -p fsonos-mcp tools::queue_add::input
```

These are test instructions, not a claim of physical-device acceptance.
