# Event callbacks across speaker networks

The daemon chooses its local callback address from the route to each player.
When different speaker networks require different local addresses, it opens a
listener on each required address and subscribes each player with its own
reachable URL. Players sharing a local address share that listener. Every bind
remains on a specific address, with the same Host checks as a single-network
installation; this does not open the control API on the speaker LAN.

`--events-port` / `FSONOS_EVENTS_PORT` applies to every callback listener.
The default is 8097. A value of `0` lets each listener choose an available port.
Allow incoming connections from the corresponding speaker networks to those
addresses and ports. Explicit seeds are still needed where multicast discovery
does not reach the speakers; callback routing does not change discovery.

Failure on one local address does not close healthy listeners on the others.
Callback processing shares the existing bounded maintenance loop, so a busy
network cannot indefinitely starve another network's events. Route changes
retire the affected subscriptions, and stable listeners retain their URLs.
An unknown player or unavailable route has no callback URL; another network's
address is never substituted for it.

An old route's listener stays open for up to six minutes after retirement, so
an admitted clip can finish using its original media URL. The daemon retains
at most 16 listeners, including retired ones; reaching that bound reports an
unavailable route and retries after capacity becomes available.

## Discovery while some speakers are offline

The LAN transport reads independent device descriptions concurrently, with at
most eight requests outstanding in one discovery pass. A slow or offline player
therefore consumes one place in that window while the other addresses continue.
Each request retains the normal per-address lane, routing, transcript and timeout;
topology reconciliation still processes results in discovery order and reports
each failure against its original address.

Shutdown or superseding a live survey stops admission of further description
reads. An already dispatched exchange keeps its ordinary timeout, and its late
answer cannot become accepted inventory after cancellation. Shared transports
and the live survey wrapper preserve this behavior. Custom transports that only
implement individual GETs retain sequential reads and their own checks, including
simulator route confinement.

The protocol's `net::description_tests` use real held localhost replies to check
overlap, ordered successes and failures, the eight-request window, continued
admission when a slot opens, and cancellation. Core regressions check live
shutdown, superseded surveys and per-address error attribution. Run:

```sh
cargo test --locked -p fsonos-proto --lib description
cargo test --locked -p fsonos-core --lib description
cargo test --locked -p fsonos-core --test survey --test survey_startup --test survey_known_sim
```

These additions passed the pinned formatter and source review. Rust execution
was still pending behind the shared build environment's dependency compilation
when this change was committed; these commands are validation targets, not a
claim that the simulator or physical speakers have passed.

## Announcements across separate networks

Speech, chimes and uploaded WAVs are generated or stored once. Each household
fetches that clip from the listener reachable by its actual announcement lead,
chosen after refreshing the target rooms' grouping. A queued announcement does
not keep using the first requested room or another household's address after
the lead has changed.

The route is resolved before the announcement suspends fades or sleep jobs,
changes levels, regroups rooms, or starts playback. A missing route fails that
household without those effects; other households in the same request can
complete. The routed URL is also the one used in playback metadata, completion
observations and ownership checks during restoration.

The daemon's synchronous, background and scheduled announcements share this
delivery path, including requests from the CLI, HTTP, MCP and browser remotes.
Direct CLI announcements open one temporary listener per distinct local route
and retain them until delivery and restoration finish. A failed lookup or bind
does not discard the other usable listeners. Existing announcement commands
and permissions remain the same.

## Keeping a coherent household during topology recovery

A full topology event is checked before it replaces the sender's household.
The sender must belong to one known household and remain addressable in the
candidate. Groups need a present, unique coordinator; physical identities and
control addresses must be unique; live and vanished records cannot conflict.
An event cannot import another known household's players or their control
addresses. An actual household transfer is reconciled through discovery.

An empty, incomplete or conflicting event leaves the previous rooms, playback
state and subscription inventory intact. The daemon requests a fresh initial
event through the existing subscription recovery path and brings discovery
forward to within 30 seconds. Failed subscription replacements keep the old
event cursor usable and honor the existing retry backoff, so repeated bad
events cannot create a subscription or discovery storm.

Ordinary peer departures, changed addresses, new coordinators, new bonded
satellites and surviving invisible stereo members still update the model.
A coherent event may omit other peers: distinguishing a real departure from
an otherwise valid omission still requires subsequent discovery.

Eight unit cases and two real-socket S1/S2 simulator cases cover admission,
preserved live state, fresh subscription IDs, failed replacement backoff and
genuine peer power-off. These additions are not yet execution-verified: the
build executor was killed during dependency compilation, and subsequent
executor failures also prevented formatting. Run:

```sh
cargo test -p fsonos-core --lib events::topology::tests
cargo test -p fsonos-core --test topology_event_recovery_sim
cargo test -p fsonos-core --test event_recovery_sim --test reconcile_sim
```

## Operating while a callback listener is unavailable

The daemon normally receives speaker state through GENA notifications. Failure
to open its local callback listener (for example, another process holding the
configured `--events-port`) no longer prevents discovery of reachable speakers.

While the listener is unavailable, the same reconciliation loop surveys the
households and reads transport, track, room volume/mute, group volume and play
mode through SOAP. Reads and changes still reach the normal live snapshot and
API event consumers. This is degraded operation, not instant speaker updates:
a successful fallback pass retries within 30 seconds (or the configured survey
interval if shorter), measured from completion. A failed discovery retains the
normal retry/backoff behavior. `Live::refresh_soon` can request an earlier pass.

With no listeners, the snapshot keeps `callback: None`, zero active
subscriptions and the actual listener error. With a partial failure, healthy
listeners and subscriptions remain available. Health reports the unresolved
failure as degraded, and `doctor` retains its detail even while another network
has working subscriptions. Local bind failure does not count as a failed speaker request.
If discovery also fails, the previous inventory/playback snapshot is retained
and the error reports both failures. No nonexistent callback is advertised and
no fake subscription success is recorded. Without a callback/media listener,
announcement clips cannot be served from that listener.

Once the configured listener can open, reconciliation subscribes
normally and clears the error. It uses the requested port, not an unannounced
replacement, and returns to the configured survey interval. No restart, new
configuration option, playback command or automatic command replay is involved.

Regression coverage in `live/eventless_tests.rs` uses an occupied loopback port,
real S1/S2 simulator SOAP and GENA endpoints, and the production `Live` worker.
Those tests cover startup, cached-only hints, control reads, retained diagnostics,
recovery on the original port, later discovery failures and shutdown. They are
not physical-speaker acceptance tests.

The multi-network additions include separate loopback listeners, route changes,
failed binds, independent media serving and subscription retirement. Their Rust
compilation and execution remain unverified in this environment: the locked
workspace check was killed while compiling the existing `asupersync` dependency,
before checking project code.

Announcement regressions in `fsonos-api/src/surface/announce/routing_tests.rs`
exercise the shared synchronous, background and scheduled paths with real media
fetches and both simulator households. Direct listener cases are in
`fsonos-cli/src/announce_cmd/listeners/tests.rs`. The new Rust tests share that
unverified status. The focused browser announcement and event-recovery suite
passed 99 tests; it does not replace Rust or physical-network acceptance.
