# Sleep timers in the native Mac remote

The Sleep panel reads the selected group's existing timer and sends set, extend,
or cancel requests through `CommandReviewSession`. The daemon or speaker owns
the timer and deadline; closing the remote does not cancel it. The native app
adds no local alarm, background execution requirement, or new daemon endpoint.

Enter whole minutes from 1 to 1380 (23 hours). A new timer replaces the existing
one; an extension adds to its current deadline; cancellation removes the timer
without resuming music. Every action displays a review with the current deadline
before a fresh confirmation and single `POST /sleep`. Typed library callers can
use durations in seconds from one second through 23 hours. Extension also checks
that the existing remaining time plus the addition fits that limit.

Timer reads always use a household-qualified `GET /sleep?zone=...`. An empty
array is no timer; an HTTP error, malformed reply, duplicate timer or wrong group
is not. The panel distinguishes an unavailable observation from a successful
empty read and drops stale timer values on failure. The acknowledgement decodes
`SleepTimerDto`, including omitted deadline/countdown fields on cancellation,
rather than assuming a generic playback outcome. It preserves the daemon's
wording and whether a fade is available; speaker-only timers never claim a fade.

Confirmation compares the reviewed and newly observed deadlines and fade state.
Ordinary countdown progress does not invalidate an extension. The daemon stores
an exact deadline. A speaker-only read reconstructs a deadline from whole seconds,
so the comparison tolerates up to one second of quantization. This is inferred
identity, not a server revision: a replacement with the same deadline and flags
cannot be distinguished, nor is the read an atomic reservation before the write.
The daemon remains authoritative for final grouping, timer and policy checks.

The countdown uses the deadline on this Mac's wall clock and is explicitly an
estimate. Its one-second screen updates do not cause network reads. Reaching zero
asks for a refresh; it never asserts that the speakers actually paused. Explicit
refresh, foreground entry, relevant live-state transitions and completed commands
read again. Failed requests never turn into an automatic set/extend/cancel.

Changing households or panels fences late reads. Cancelling an unsent review or
suspending the screen prevents dispatch and retains the busy slot until reads
drain. A sent timer change can still take effect; a late receipt remains labelled
with its original household and group. Malformed or lost acknowledgements stay
uncertain and consume the review, so a second confirmation cannot repeat them.

## Checks

Run `swift test --package-path apps/apple -Xswiftc -warnings-as-errors`.
Twenty-two sleep tests and the twelve scene tests execute the production client,
request/response codecs, bounded state reads, command actor and screen controller
with synthetic daemon replies. Coverage includes successful set/extend/cancel,
countdown progress, replaced deadlines, speaker-only receipts, same room names in
two households, failed/contradictory reads, late replies, suspension, and no polling
of a quiet timer panel. These are client-contract tests, not tests of actual
speaker timer expiration. Apple SDK compilation/package tests run separately in
the existing native workflow; real LAN playback and tailnet checks remain opt-in.
