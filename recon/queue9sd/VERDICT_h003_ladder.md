# H-003 wire test verdict — .84, 2026-10-29 04:07–04:32Z (approved window)

Setup: AddURIToQueue via curl (python sockets to LAN broken this session),
`EnqueuedURIMetaData` DIDL with `dc:title` of the stated size, track URI served
from `.160:8903`. Independent 0.1 s `/status/zp` SWGen poller per the new
doctrine (not hls_det). Queue-clear attempted via `RemoveAllTracks` — the S1
Queue:1 service rejected every shape (401/402 on InstanceID/QueueID variants),
so the cleared-queue control ran on the post-settle state (fresh boot-state
queue, no items) and the final shot ran on the ACCUMULATED 4-item queue
(stateful arm). Settle + Stop between shots. 3-minute spacing.

## Verdict table

| shot | dc:title | queue state at write | AddURIToQueue | SWGen before | death in 72–97s window? | SWGen after |
|---|---|---|---|---|---|---|
| 0 (control) | 60,000 B | post-settle, empty | 200 | 1 | **no** (90 s watch clean) | 1 |
| 1 | 8,192 B | accumulated (1 item) | 200 | 1 | **no** (90 s watch clean) | 1 |
| 2 | 30,720 B | accumulated (2 items) | 200 | 1 | **no** (90 s watch clean) | 1 |
| 3 | 61,440 B | accumulated (3 items) | 200 | 1 | **no** (90 s watch clean) | 1 |
| 4 (stateful) | 61,440 B | accumulated (4 items) | 200 | 1 | **no** (150 s watch, clean at +3 min) | 1 |

Logs: `recon/queue9sd/logs/h003_t{0..4}_*.log`. Final device state: SWGen=1,
`57.23-74170`, settled + Stopped at 04:32Z.

## Decision-matrix outcome (model rubric)

**"no death at any size" branch.** The 72–97 s deferred kill did NOT reproduce
on `.84` under the ladder: five shots spanning 8 KB → 60 KB, two queue states
(empty / accumulated 4-deep), independent 0.1 s polling through the entire
documented kill window plus margin — zero SWGen changes.

## Interpretation against the model

- The kill recorded 2026-10-22 (8/16/32/60 KB on Play:5, 32 KB on One) is
  **state-dependent beyond mere queue non-emptiness**. Accumulating 4 items
  with multi-KB titles did not recreate it.
- Unverified variables that remain candidates for the original trigger:
  (a) the original runs' `<res>` metadata shape (the 2026-10-22 test used the
  same DIDL in SetAVTransportURI, where the title stores truncated at 629 B —
  the queue item may have inherited a *different* record type),
  (b) the coordinator/queue pairing at the time (stereo-pair state),
  (c) the 86.10 One (32 KB kill) — S2's queue record layout is its own RE task.
- The model's async-walk hypothesis is **unfalsified but unconfirmed**: no
  death ⇒ no crash-time observation; the walk's title handling remains the
  open static question (untruncated-length consumer).

Honest status: the wire test is a clean **negative reproduction attempt** on
this unit/build/state — not a refutation of H-003 (the original evidence was
live and twice-generation). Follow-up arms if the window reopens: replay the
EXACT 2026-10-22 DIDL shape (same `<res>`/class), same on the One (.86-class,
S2 setter path), and the [short,multi-KB] ordering arms from the model.
