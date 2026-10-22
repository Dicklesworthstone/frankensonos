# Move native queue entries together

In **Up next**, choose **Select entries to move together**. Select individual
occurrences or **Select this page**, then use the existing Previous and Next
buttons to select across pages. The native remote retains at most 100 selected
entries, not a second copy of the entire queue. Selection alone never plays,
seeks, removes or moves anything. While selecting, ordinary row playback,
queue search actions and the source-entry controls are unavailable.

The selected positions must form one consecutive block. Gaps are not filled
implicitly, duplicate recordings remain distinct occurrences, and entries with
missing renderer identities cannot be selected. Select this page is all-or-
nothing: it does not silently skip an unknown entry or exceed the limit.

Choose **Review moving selected block**, then earlier, later, after the current
entry, start, or end. Earlier/later moves the entire block one position, keeping
its internal order. Unavailable or no-op destinations are disabled. After-current
requires a known current slot outside the selected block and checks that slot
again before confirmation. Absolute destinations allow normal playback progress.

The ordinary command review lists the original range, every selected entry,
and the intended placement. Confirmation uses the same one-use command session
as playback and single-entry moves. Preparation and confirmation each reread
all selected occurrences in up to four sequential 25-entry requests, checking
their original positions, exact renderer URI bytes, total and opaque revision.
A mismatch on any page aborts the complete move before a write. An incomplete
response cannot produce an unbounded page loop or a partial-block mutation.

Exactly one existing `/queue/move` request carries the original first position,
ordered `expected_uris`, insertion coordinate and revision. The daemon performs
the revision-checked native reorder. The client never clears/rebuilds a queue,
re-enqueues tracks or loops through single-track mutations. Playback is not
started or restarted. **Queue moves cannot be undone.** Client preflight is not
an atomic reservation; the daemon's final revision check remains authoritative.

Selection can survive page navigation only while group, queue length and
revision match. Refresh, connection/catalog invalidations, failed reads,
regrouping, leaving the panel or backgrounding retire selections and captured
menus. Changed playback position retires the old placement menu without silently
changing a reviewed command. Repeated copies are identified by original position
and URI, not deduplicated. Search results are never treated as consecutive pages.

Acknowledgement must confirm the complete count, resulting position, target,
total and a changed revision. A partial, contradictory or lost reply remains
uncertain and is never automatically repeated. An already-sent move cannot be
recalled by closing the screen or cancelling its network request.

## Verification boundary

`QueueBlockTests.swift` tests range arithmetic, all five destinations, duplicate
occurrences, overflow, full-range receipts, bounded sequential preflight,
cancellation and cross-page selection. The focused Swift 6.2.1/Linux harness
runs these tests with warnings treated as errors using the exact production
helpers and relevant domain declarations; it is not the complete application.

`QueueBlockIntegrationTests.swift` exercises the real shared command session,
S1/S2 request fields, changes on later pages, the held-read cancellation gate,
and uncertain single-shot acknowledgements. These additional integration tests
are committed for the full native package; they were not executed in the focused
harness. Apple SDK type-checking, rendered selection UI, full-package regression
results, live daemon integration and physical Sonos acceptance remain separate
checks. Parsing a SwiftUI source file is not an Apple SDK build.
