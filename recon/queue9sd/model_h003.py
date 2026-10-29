#!/usr/bin/env python3
"""9sd: model of the H-003 deferred queue-dc:title kill (frankensonos-9sd).

Known measured facts (docs/PROTOCOL.md, docs/RE_COPY_AUDIT.md, bead comments):

  F1  AddURIToQueue with EnqueuedURIMetaData DIDL whose dc:title is 8-60 KB:
      SOAP answers 200 immediately; the daemon dies 72-97 s LATER; watchdog
      restart; recovery 39-225 s; ICMP up throughout. (Play:5 57.23 + One 86.10)
  F2  State-dependent: a 60 KB item on a JUST-CLEARED queue survives.
  F3  Element-specific: the same size inside <junk> instead of <dc:title> survives.
  F4  Not an inline strcpy: deferred/asynchronous work against the STORED title.
  F5  SetAVTransportURI with the same metadata stores only 629 bytes of title
      (measured) - the queue path is the one that keeps the full title.
  F6  Queue store-time cap: none. CreateSavedQueue stores a 2139-byte title;
      the BROWSE readout truncates at 1023 bytes (store-vs-read split measured).
  F7  Browse of a queue holding such an item takes >45 s to answer.

Static anchors (this lane + prior):
  A1  The queue record keeps the full title (F6); no store-side bound found.
  A2  The 2.1c reader (0x101cc878) shape: copy length derived from the STORED
      string (strrchr span / stored length word at obj+4208), destination a
      fixed frame buffer - length and capacity come from different objects.
  A3  9sd sweep: 0x101a039c/0x101a04d4 unbounded path-element copies exist in
      the same CIFS/browse family (length = strrchr distance, no bound check).

Model of the deferred kill (all five facts fit one mechanism):

  t0     AddURIToQueue stores the queue record with the FULL multi-KB title,
         answers 200. Inline copies are bounded (F1's immediate 200).
  t0+~90  the periodic queue-maintenance pass runs (queue-db flush / art
         prefetch / browse preparation). It walks items and copies each title
         into a fixed buffer. The copy length comes from the STORED string
         (A2 shape), but the buffer/reuse discipline is stateful:
           - empty-then-one-item (F2): the pass handles the single item with
             a freshly sized buffer -> survives.
           - the killed sequence: the pass interleaves two items (or reuses a
             cursor sized by the previous item) -> the multi-KB title is
             copied into a buffer sized for the previous/next record.
  F3 fit: only dc:title reaches that record field; <junk> content is dropped
  at DIDL parse time and never stored on the queue record.

  F7 fit: browse >45 s is the same walk - the queue enumeration IS the slow,
  title-heavy pass; 72-97 s is its first scheduled run after the enqueue.

Device discriminator (cheap, one window, coordinator .84):
  queue = [short-title item, multi-KB item]  vs  [multi-KB item, short-title item]
  vs    [multi-KB item alone on cleared queue (control, known survive)]
  Order-dependent survival confirms the interleave/reuse model and pins the
  reader. All three arms use identical title bytes; only ordering changes.

Why the async worker and not the inline parse: F1 answers 200 instantly and
the death is minutes-scale periodic; the parse path was swept by 9sd (1534
memcpy sites) with the queue-inline copies bounded.
"""

## Static refinement (2026-10-29, post-model)

- Queue-API record (type 0x98 in the descriptor table at 0x1044cc34, routes
  `players/{playerId}/upnpQueue`): title capacity **629 bytes** (`li r6, 0x275`
  at 0x102360e4, serializer family 0x10219464/0x10219cfc). This is the
  measured SetAVTransportURI 629-byte title store.
- TLV blob record: length word obj+0x448 (writer-guarded 0 < len <= 0x400 at
  0x10224c44), buffer obj+0xe58 capacity 0x458; consumer 0x10219954 checks
  the bound AFTER the copy and only logs — overrun up to ~0xF6 bytes past the
  buffer (sum of 7 + title-len + blob-len vs 0x458). Known, log-only.
- The .rsq file writer (fcn.101df010, `<Track URI=` / ` MD=` at 0x101df514/
  0x101df87c) appends through a capacity-checked dynamic string
  (0x1038d128 compares buf+0x10+len+1 vs buf+0xc; 0x1038c788 = strlen +
  vtable+0x10 appender). **Write side bounded** — the .rsq file is not the
  overflow.
- Therefore H-003's multi-KB dc:title must reach a consumer that uses the
  UNTRUNCATED length; the only such site is the async walk (browse prep /
  maintenance). The device ladder (8K/30K/60K) discriminates: kill latency or
  probability scaling with length => unbounded copy in the walk; binary
  behavior => state flip in the walk.

Device harness staged (pending window): `/tmp/h003_shot.sh` (AddURIToQueue via
curl — python socket to LAN still broken this session — plus an independent
0.1 s `/status/zp` SWGen poller per the new doctrine, 90 s max), queue-clear +
settle + Stop between shots (`/tmp/h003_settle.sh`), track URI server on
`.160:8903`.

## Wire-test decision matrix (pre-computed, for the 8K/30K/60K ladder)

| observation | mechanism | next |
|---|---|---|
| death at all 3 sizes, latency ~constant (~72-97s) | state flip in the walk (title length irrelevant beyond presence) | diff the walk trigger |
| death probability/latency scales with length | unbounded copy in the walk (copy size <-> walk window race) | pin the copy: 9sd sweep
  candidates 0x101a039c/0x101a04d4 or the 0x101cc878 path-B reader |
| no death at any size (3/3 clean) | F2 generalizes: cleared-queue control required; the killed
  sequence needs specific prior queue state | ladder the prior state: [short,item] vs
  [item,short] vs [item,item] |
| death only after Stop/requeue or second enqueue | the walk runs on queue-structure change,
  not on a timer | instrument the walk trigger via the .rsq file mtime
  (savedqueues.rsq write = walk ran) |
SWGen is the crash oracle: any change 0->1 means the daemon died and the
watchdog restarted it; the 0.1 s poller bounds the death time to +-0.1 s.
