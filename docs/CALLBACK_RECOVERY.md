# Operating while the event callback is unavailable

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

The snapshot keeps `callback: None`, zero active subscriptions and the actual
listener error. Local bind failure does not count as a failed speaker request.
If discovery also fails, the previous inventory/playback snapshot is retained
and the error reports both failures. No nonexistent callback is advertised and
no fake subscription success is recorded. Without a callback/media listener,
announcement clips cannot be served from that listener.

Once the configured listener can open, the next successful survey subscribes
normally and clears the error. It uses the requested port, not an unannounced
replacement, and returns to the configured survey interval. No restart, new
configuration option, playback command or automatic command replay is involved.

Regression coverage in `live/eventless_tests.rs` uses an occupied loopback port,
real S1/S2 simulator SOAP and GENA endpoints, and the production `Live` worker.
Those tests cover startup, cached-only hints, control reads, retained diagnostics,
recovery on the original port, later discovery failures and shutdown. They are
not physical-speaker acceptance tests.
