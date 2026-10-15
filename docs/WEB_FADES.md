# Native fades from the web remote

Open **Volume fades & gentle pause** in the web remote. This panel uses the
existing public fade API, caller identity, house policy and action history.
It creates neither a sleep timer nor a schedule. Closed panels do not poll.

## Fade one speaker

Choose **Fade one speaker to a volume**, select any room (including a group
member that is not the coordinator), and read its current state. Household
names distinguish identically named rooms. The volume draft starts at the
speaker's observed level, not a guessed default. Choose a new level from 0 to
100 and a duration from 1 to 60 seconds, then **Review fade**.

Moving the slider, typing, reviewing and cancelling never write to a speaker.
**Confirm fade** sends one `POST /volume/fade` containing the exact qualified
room, requested volume and duration. The daemon performs the fade; the browser
never approximates it with a series of volume commands. Muted speakers stay
muted and paused music is not resumed. Other group members are not targeted.

The server's house caps and step limits still apply. Acknowledgements show the
server's notes, including a clamped target or a superseded action; they do not
cause another write to force the original target. An acknowledged no-op is
also a valid result. Another volume action can supersede a running fade.

## Gently pause a group

Choose **Fade out and pause a whole group**. Choices name every member of the
group being paused, with the household, rather than implying that only the
selected coordinator changes. Read the current state, select a duration, and
review the group before confirming.

One `POST /pause/fade` asks the daemon to fade the group out, pause it, then
restore its pre-fade volume levels. It does not send a resume, unmute, queue
replacement or follow-up volume command. The panel requires a known, playing
source; paused, stopped, transitioning or unknown playback cannot be resumed
by this control. A failed fade or restoration can leave a partial result.

## Review, freshness and uncertain outcomes

Every review freezes the target and typed request. Editing, cancelling,
changing modes, refreshing, switching rooms or closing consumes that review.
A detached confirmation button cannot confirm a different review. The panel
allows only one pending native action, including across close/reopen.

Before dispatch, it re-reads the inventory and the selected speaker's state.
A missing speaker or changed volume/mute refuses the action. A gentle pause
also requires the same coordinator and full member set, the same playing
source URI and the same queue position (so repeated tracks remain distinct).
Normal elapsed-time progress does not invalidate a pause. A per-speaker volume
fade may follow that speaker through regrouping because its scope is still
only that speaker.

These browser checks are **not an atomic lock** on the speaker or group.
Another controller can change state between a read and the native command.
Policy and fade orchestration remain the server's responsibility.

No write is automatically replayed. An unreadable reply or lost connection
may mean a partial fade, a completed fade, or an action still running on the
daemon. Closing the browser does not cancel it. Read current state and check
the action history before another fade; a fresh read alone does not prove a
previous timed-out operation has stopped. The panel also requires a new read
after success rather than pretending its requested level is current state.

## Verification

`node --test scripts/test-remote-fades.cjs` runs 31 dependency-free controller
regressions. They cover native single dispatch, exact room/group scope,
review consumption, changed membership/source/duplicate queue entries,
partial input, volume/duration bounds, muted and paused speakers, policy
notes, stale reads, close/disposal races and malformed or lost replies.
The existing Linux workspace workflow runs this suite alongside sound tests.

Chromium checks at phone and desktop widths exercise the shipped panel with
API fixtures: local sliders, review/cancel, keyboard focus, stale group
refusal, policy-limited results, gentle pause, text-only metadata and layout.
These fixtures do not establish Rust HTTP integration, physical speaker
behavior, fade timing or successful hardware restoration.
