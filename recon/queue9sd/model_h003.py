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
