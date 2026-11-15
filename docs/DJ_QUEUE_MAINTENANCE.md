# Continuous DJ queue maintenance

The DJ retires completed works once its queue exceeds `trim_queue_at`, keeping
`keep_played` tracks before the observed playing position. Retirement uses the
same `fsonos_core::control::queue_removal` implementation as manual queue edits
and logged enqueue undo. There is no separate interpretation of a deletion's
acknowledgement in the automatic feed.

## Playback limits survive a restart

The daemon saves the policy identity of whoever starts a DJ feed in the same
atomic `dj-feeds.json` revision as its queued works and group membership. Before
automatically restarting playback, it applies both the DJ's current policy and
the initiating caller's current policy to every room in the current group.
Changes to room caps and quiet hours therefore apply to a resumed feed too,
including when the owner has exempted the `dj` client from volume caps.

A start that queues music and then fails still retains its caller. If a failed
restart leaves an existing feed active, both the previous and incoming callers'
limits remain in force; a successful explicit start establishes the new caller
alone. The stricter identities are saved before engine start or queue mutations.
If that save fails, the engine does not start; the surface may already have
lowered rooms to the caller's safe volume. If the final save fails after music
starts, the earlier revision still includes the incoming caller's limits.
Policy identities follow the feed through explicit moves and spontaneous
coordinator changes, and disappear from saved state when ownership ends.

Old saved feeds have no initiating identity. Their queues and status remain
available, and already playing music can still be observed and refilled, but
automatic restarts wait. Run `fsonos dj start ROOM` once to establish the caller
and current limits. A failed attempt preserves that wait. Only policy identities
are stored here; volume limits are read afresh from `policy.toml` when needed.

An exhausted queue also waits when live group membership no longer matches the
household snapshot used to apply caps. After that snapshot catches up, recovery
can continue. A group or player boot change during observation or refill
invalidates that attempt without replaying accepted queue additions.

## Listening evidence stops at observation gaps

Degraded or offline player health interrupts that coordinator's implicit
listening evidence before any topology handover or feed work. A live stream
resync interrupts every retained feed, including a coordinator absent from the
current topology. Time spent without reliable observations cannot turn into a
full listen or an early-skip preference after connectivity returns.

This interruption performs no speaker I/O and retains the owned queue,
steering, initiating policy identities and pending refill retries. Maintenance
waits while the coordinator's current health is degraded or offline. Healthy
alone does not end the gap: a volume event or subscription renewal can mark a
player healthy while its playback snapshot remains old. Before feeding again,
the DJ requires a complete renderer read to agree with the retained transport,
source, track, queue position, length and any known play mode. An unknown cached
renderer status requires a fresh `OK`; it cannot hide `ERROR_OCCURRED`. The
first resumed sample also uses the freshly read position to rule out a false
early skip after a same-slot seek. The read brackets position with transport
and source checks; mismatches and failures retain the
gap and back off, without volume churn repeatedly querying the speaker. The
pending proof follows the feed through coordinator handovers.

A confirmed observation starts fresh evidence; a later work observed from its
beginning can still earn a full listen. Ordinary queue-content
notifications remain separate: the DJ's own appends preserve listening
continuity when reconciliation confirms the same occurrence survived.

The regression coverage includes elapsed-time feedback checks, both simulator
generations, and follower dispatch before topology and playback:

```sh
cargo test --locked -p fsonos-cli --bin fsonos dj::feedback::tests
cargo test --locked -p fsonos-api --lib surface::dj_follow
```

## Renderer failures do not cycle back into new picks

When a confirmed `ERROR_OCCURRED` advances the DJ to the next queued work,
the failed track waits 30 minutes before it can enter a new selection. Every
recording containing that track waits as a whole; another recording of the same
piece remains available. This prevents a track that failed before its first
successful listening event from looking fresh and being queued again immediately.
A small shelf can keep rotating through its remaining playable recordings.
Steering relaxation, repeat avoidance and rotation never bypass this wait.

These exclusions belong to the feed, with at most 64 track identities retained.
They survive a daemon restart and follow the feed when playback moves. The
existing recovery checkpoint saves them with the attempt budget before any
recovery command; a failed checkpoint sends no command and leaves the live feed
unchanged. They do not change the library, preferences, feedback or play history,
and do not remove or reorder music already on the speaker.

If no matching recording remains available, new selections return `NotReady`
with the next exclusion deadline while existing playback continues. Automatic
refill uses its normal bounded retry schedule, and expired tracks may be selected
again. `dj skip` preserves the exclusions while resetting the recovery attempt
budget; an explicit `dj start` clears them to allow a deliberate retry. Rehearsals
use the same exclusions and evaluate expiry at each projected pick's time.

Regressions cover immediate refills on S1 and S2, snapshot restart, retained
playback when the pool is temporarily unavailable, expiry, skip/start behavior,
alternate recordings, strict/adaptive selection and rehearsal time. Run:

```sh
cargo test --locked -p fsonos-spotify --lib feed::tests::recovery
cargo test --locked -p fsonos-spotify --lib feed::recovery::tests
cargo test --locked -p fsonos-spotify --lib dj::recording_tests
```

The pinned formatter passes for these additions. Their Rust execution remains
unverified: the workspace dependency build was killed in `asupersync` before
reaching the changed crates. Physical-speaker acceptance remains separate.

## Current preferences and library

An uninterrupted DJ picks up edits to `preferences.toml` and `moods.toml`
without a restart or skip. Automatic playback processing shares one file check
across every room, at most once per 30 seconds, including the daemon's quiet-queue
maintenance. The next successful rebuild applies the new settings to future
picks; music already queued keeps its whole-work order and the current track
is not restarted.

Moods, preferences and the library pool become active together after validation.
If an edit is incomplete or invalid, automatic feeding retains the last valid
plan and retries after its existing 30-second backoff. Ordinary volume events
do not trigger repeated full-library rebuilds. Status reports the effective
validated programs while a replacement is pending; reading status or preferences
does not partially replace that live plan.

Rehearsal, setting steering and explicit like/dislike commands check current inputs
immediately, including a completed library sync and the latest feedback. Rehearsal changes
no speaker state, saved feed, play history or feedback. A newly synced song can
receive feedback without first restarting the DJ. Invalid configuration returns
an error to these explicit requests while leaving the previous live plan intact.
A rejected steering change also preserves the previous session, so a new mood
cannot become active without its validated preferences. Clearing steering remains
available while files are invalid and leaves pending library refreshes for the
next automatic processing.

The focused regressions use the in-memory store and the real loopback simulator:

```sh
cargo test --locked -p fsonos-cli --bin fsonos dj::plan_cache_tests
```

## One verified deletion contract

Every removal selects complete, contiguous works behind the retained boundary,
reads a coherent queue revision, and matches the claimed tracks at their saved
positions. A fresh playback/source observation prevents an already-observed
backwards seek or source replacement from being treated as permission to trim.
The shared core checks coordinator ownership, sends one nonzero-revision range
removal, and verifies the complete resulting queue and coordinator again.

A successful reply alone is not enough: the observed queue must have exactly
the selected range removed, a changed revision, and unchanged prefix and suffix
URIs. Lost or malformed acknowledgements can succeed from that same evidence
without repeating a write. Explicit SOAP refusals remain errors. No queue
clear/rebuild, source switch, play, seek, volume or grouping operation is added.

An uncertain result, including an acknowledged deletion with contradictory or
unreadable read-back, drops the old cleanup claims before resynchronization.
Those dropped claims are absent from subsequent feed snapshots too. Positions
remain untrusted when resynchronization fails, so automatic top-up waits for a
later observation rather than completing a partial work from stale positions.

## Adjacent works share a request

Consecutive complete work claims at adjacent queue positions are coalesced.
For example, 120 completed adjacent songs can retire in one range deletion,
with one pair of complete before/after queue observations, instead of a range
request and queue scans per song. This is a change in the operation structure,
not a measured wall-clock speedup or a guarantee about speaker latency. The
protocol reader still pages large queues and final feed reconciliation still
reads the queue.

A partially queued work, an unowned entry between claims, or a work crossing
the retained boundary is never swept into a combined range. Missing positions
and overflowing ranges are ineligible; overlapping or reordered eligible
claims do not authorize cleanup. Retained complete works participate in the
overlap check too, so an eligible claim cannot delete part of a retained work. Batches stay within the protocol's object
limit and run back to front so removing a later range does not renumber an
unprocessed earlier one. Movement metadata is borrowed, not cloned or sorted.

At most eight batches are considered per playback event. Remaining old claims
stay in the feed and later events can continue retirement after reconciliation.
This bounds the number of deletion batches, not total elapsed time: individual
network calls still have their existing timeouts. The DJ's queue threshold,
lookahead, work selection, feedback and play-history policy are unchanged.

## Boundaries and verification

This does not make cleanup transactional with other controllers. Queue
revisions protect the queue edit, not a concurrent source/seek change after
preflight. Saved claims identify URI/position occurrences, not permanent queue
item identities; identical replacements before observation cannot always be
distinguished. A played work whose tracks resynchronization finds more than
once at or before its saved position (the owner queued the same song) loses its
claim rather than guessing which copy is the DJ's, so cleanup leaves both.
Cleanup never gains permission to search another coordinator's queue for the
same songs.

Seven simulator tests cover shared-verifier integration and the deliberately
false acknowledgement fixture. Seven pure batching tests and seven additional
simulator tests cover whole-work batching, owner
gaps, incomplete work, a 120-song range, eight-batch yielding and continuation,
a changed member of a batch, and a backwards seek. The existing adjacent-work
lost-reply test now requires one combined removal, while retaining its queue,
position and no-replay assertions. Run:

```sh
cargo test --locked -p fsonos-spotify --lib feed::trim
cargo test --locked -p fsonos-core --lib control::queue_removal
cargo fmt --check
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
```

These commands are the Rust and simulator proof for this behavior; physical
speakers and the live Spotify service remain separate acceptance checks.
