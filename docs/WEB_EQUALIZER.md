# Speaker sound from the web remote

Open **Speaker sound**, choose a room and read its current sound. Every visible
member of a group is selectable, not just the coordinator. Room choices include
the household so identically named rooms in different households stay distinct.
The panel calls `GET /zones/{room}/eq` and `POST /eq`; it never contacts a player
directly and adds no new control endpoint or privilege.

## Tune one speaker

Bass and treble range from -10 to +10, with 0 flat. Loudness compensation boosts
bass and treble at low volume. Moving a slider or toggling loudness only edits
a local draft. **Prepare flat sound** sets the draft's bass and treble to zero
and loudness off; it also does not send a command. **Discard edits** restores
the last read values without changing the speaker.

**Apply sound** sends only the fields actually edited, including explicit zero
and false values. It tunes the chosen speaker even when that speaker follows
another group coordinator; it does not change group membership, playback,
volume or the other members' sound. The existing server policy, action history
and tone undo apply. A server failure part-way through a combined edit can
leave a partial change; consult the action history for its undo.

## Freshness and failures

Immediately before dispatch, the panel re-reads the room list and that
speaker's tone. A missing room or a changed bass, treble or loudness value
refuses the whole edit without a POST. These are read-before-write checks,
**not an atomic reservation**: another controller can still change the speaker
between the check and the write. The existing API remains the authority.

At most one sound write is pending in the panel. It is never automatically
repeated, even after a network failure or unreadable reply. An uncertain or
mismatched acknowledgement discards the draft and requires **Read current
sound** before another edit. A fresh read cannot prove that an older timed-out
server operation has stopped; check the speaker when the outcome is uncertain.

Room changes, refreshes and closing discard drafts. Late reads cannot repaint
a different room or a closed panel. Closing during preflight cancels the intent
before dispatch; closing after dispatch does not cancel a speaker operation or
replay it on reopening. Closed panels do not poll.

## Verification

`node --test scripts/test-remote-equalizer.cjs` exercises the shipped controller:
27 regressions cover grouped members, duplicate room names across households,
strict response and input validation, changed fields only, stale preflight,
asynchronous read ordering, double-submit suppression, closing/disposal,
partial/uncertain replies and local-only edits. It needs no JavaScript packages
and runs in the Linux workspace workflow.

Chromium checks with API fixtures cover phone and desktop layouts, sliders
without replacing the active control, keyboard focus, stale-state refusal,
flat sound, discard, exact per-member writes and text-only room metadata.
These checks do not replace Rust HTTP/simulator or physical-speaker tests.
