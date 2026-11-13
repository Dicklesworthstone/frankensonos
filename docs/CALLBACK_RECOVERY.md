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
