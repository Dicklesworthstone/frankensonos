# Listening history

`fsonos history`, HTTP `GET /history`, and MCP `recent_plays` read the same
stored listening history. The daemon records music it observes playing,
including playback started from another controller. The DJ uses those records
for its listening history and repeat avoidance.

Each record identifies the group coordinator, the source URI, and when the
play was observed. Spotify renderer URIs are normalized to their `spotify:`
form. Available titles and artist credits are retained so streams and music
outside the cached library can still be named. A record is evidence of an
observed play, not proof that the complete track was heard.

## Startup and event recovery

The history follower subscribes before reading the current live snapshot. It
records music already playing when the daemon starts even if the speakers send
no further change. When that coordinator's latest stored play has the same
source, it is treated as the continuing play from the previous daemon run.

A lost-event notification causes every current coordinator to be reconciled
from one snapshot. Topology changes and recovered players also trigger
reconciliation. A delayed event from a former coordinator cannot record its
stale playback as a new listen.

Recovery records the current observation. It cannot reconstruct tracks that
started and finished entirely during downtime or an event gap. Likewise, a
restart cannot prove whether the same source was restarted while the daemon
was away; matching the latest stored source preserves continuity.

## Repeats and groups

A known queue-position change distinguishes consecutive occurrences of the
same source URI. Each observed occurrence can be recorded even if the previous
one played within the short window used to deduplicate concurrent command and
DJ writes. Learning a previously unknown queue position, pausing and resuming,
or receiving new metadata for the same occurrence does not add another play.

Independent groups each get their own record, including when groups in S1 and
S2 play the same source. A matching URI alone is insufficient evidence that
music moved. Handover deduplication requires a newly acquired coordinator role
and overlap with the members of a previously observed group. Prior membership
is retained for two minutes so a topology-first group split can be matched
when the new coordinator's playback arrives. The exception is consumed by
that first observation; later repeats are separate plays.

Queue positions are the identity available in the live model, not permanent
queue-entry identifiers. A queue edit that renumbers the currently playing
entry can therefore look like a new occurrence. History does not poll or edit
the queue to infer a stronger identity.

## Store recovery

An observation is acknowledged only after its store operation succeeds. A
temporary store failure leaves the current play eligible for another attempt.
The follower revisits cached state every 30 seconds as well as responding to
events, so recovery does not require a new speaker notification. These passes
perform no discovery or playback reads against the speakers.

## Regression checks

The focused unit tests in
`crates/fsonos-api/src/surface/heard/tests.rs` cover snapshot recovery, lost
events, repeats, independent groups, coordinator lineage, and retry after
failed store access. `crates/fsonos-api/tests/history_heard_sim.rs` exercises
the follower with virtual players over real localhost sockets, including
startup with music already playing, duplicate queue occurrences, Spotify
Connect tracks, pause/resume, restart continuity, and group handover.

```sh
cargo test -p fsonos-api --lib surface::heard::tests
cargo test -p fsonos-api --test history_heard_sim
```

These are software regressions; they do not substitute for live checks on the
owner's physical speakers.
