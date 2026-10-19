# Activity and listening history in the native Mac remote

Choose **Activity & History** in the connected remote's toolbar. It works without
a selected speaker. The action log and listening history are separate reads of
the existing daemon store endpoints, not a second logger, a subscription to all
speaker traffic, or a complete listening archive. The panel starts no playback,
changes no policy and never treats a log entry as an acknowledgement of an
uncertain command.

## Inspect recorded activity

The action log preserves the daemon's exact Int64 identity, timestamp, client,
surface, intent, policy decision, result, and optional undo reference. The
before-state indicator describes what was recorded; it is not a promise that a
particular row is currently eligible for undo. This panel has no Undo button:
the current daemon undo operation chooses its newest eligible action, not an
arbitrary selected record.

Listening history shows the recorded source, timestamp and any known title,
artist and coordinator-room label. The API has no stable play-record or household
ID. Repeated identical records remain separate entries, and missing metadata is
not invented. Historical room labels may be absent or ambiguous across households;
they are not playback destinations. A record does not prove a complete listen.

Filters are applied explicitly: exact client and optional starting time for the
action log, a household-qualified group for history, and a maximum of 25, 50, 100
or 200 rows per feed. These are the newest matching records, not paginated full
archives. The start-time editor and displayed timestamps use this Mac's local
time zone; the API receives Unix seconds. Search affects only the loaded records
and does not send requests. Literal plus signs, ampersands, Unicode and other
query data remain data through the daemon's form-style decoder.

Both feeds run independently with at most two reads in flight. Each shows its
own last-successful-read time and error. A failed refresh retains its old rows
as explicitly stale; a failed first read never masquerades as an empty successful
history. Changing server filters clears the old query's rows and fences delayed
responses. Closing or backgrounding cancels outstanding reads; rapid reopening
waits for cancelled I/O to drain. Refresh bursts coalesce to one following batch.
There is no idle polling or automatic retry loop.

## Review a recorded source for playback

Select a portable source in Listening history, explicitly choose a current group
and household, then choose **Play now**, **Queue next**, or **Append to queue**.
**Review in remote** closes Activity, selects that chosen group, and opens the
existing native command review. Nothing is sent until its separate confirmation
and fresh state checks succeed. Changing the destination's membership or
coordinator invalidates the handoff rather than selecting a replacement group.
Closing Activity without this explicit action does not open a command review.

The history model offers canonical Spotify track/album/playlist URIs and ordinary
HTTP(S) sources without embedded credentials or fragments. It does not guess how
to translate renderer-specific Sonos/Spotify URIs, recover a whole work from one
movement, or reuse an old queue position. Unsupported sources remain readable.
History refresh, filter changes, and suspension invalidate old selection tokens.
The destination always comes from the current house, never the historical label.

Play can replace the chosen group's current source and queue. Queue operations
use the existing daemon behavior and do not start playback. All three use the
same RemoteModel and CommandReviewSession path as the existing controls; this
feature adds no writer, request replay, or new daemon endpoint. A cancelled
request is not Undo. The daemon remains authoritative for policy and final
concurrent device changes.

## Checks and remaining acceptance

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
Twenty-five new tests cover exact queries, full-width IDs, missing and repeated
history entries, bounded reads, filter changes, isolated failures, cancelled
requests, rapid reopen, current-target selection and all three replay actions
through the existing command actor. The plus-encoding regressions fail against
the previous encoder and pass with this change.

Local Swift 6.2.1/Linux checks use production sources and exact relevant ranges
of the existing client/command implementation, not the full upstream package.
A separate real localhost HTTP smoke check exercises the production URLSession
transport, form decoding, history-to-queue handoff, denial and an unfollowed 307.
Its server is synthetic. The native Mac workflow is the full package and Apple
SDK build gate. These checks do not establish interactive Mac behavior, real
daemon history, physical-Sonos playback, Tailscale access or Rust-workspace health.
