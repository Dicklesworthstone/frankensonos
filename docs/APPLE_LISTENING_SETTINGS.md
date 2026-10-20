# Native speaker sound and queue playback order

The iPhone, iPad and Mac apps expose the daemon's existing EQ and play-mode
controls through the same SwiftUI sources and shared command-review session.

## Tune one speaker

Open **House → Speaker sound**. In a multi-room group, explicitly choose a
speaker. Bass and treble run from -10 to +10; zero is flat. Loudness compensates
bass and treble at low listening levels, not the volume control. **Choose neutral
sound** prepares zero bass, zero treble and loudness off as a local draft.
Nothing is sent while dragging, toggling, choosing a speaker or opening a panel.

**Review changes** shows the exact household-qualified member, observed tone
and requested tone in the existing confirmation sheet. Preparation and final
confirmation each re-read the tone and group. Only edited tone fields go to
`POST /eq`; this does not send a volume, mute, playback or regrouping command.
The target is the chosen member, even when another speaker coordinates its group.

A failed multi-field speaker adjustment can be partial. Lost, malformed or
contradictory responses stay uncertain and are not retried. The result keeps
its original member identity. Activity may offer the daemon's reviewed undo;
there is no automatic rollback or guaranteed undo for every failure.

## Shuffle and repeat without restarting music

Open **Music → Up next → Shuffle & repeat**. Shuffle and Repeat are separate
native controls, but their draft produces one complete backend mode. All six
combinations are supported. In particular, shuffle with repetition off is
`SHUFFLE_NOREPEAT`; `SHUFFLE` means shuffle with whole-queue repetition.

The review carries the observed `expected_mode` to `POST /play-mode`. The
current group, selected queue, mode and DJ ownership are checked again before
sending. Natural track advances do not invalidate an otherwise unchanged
queue-mode review. Applying a mode does not start paused music, switch from
radio/line-in to the queue, change volume, or reorder the stored queue entries.

An active whole-work DJ permits only sequential NORMAL mode. The interface
explains that restriction rather than silently stopping the DJ. An inherited
nonsequential mode can be reviewed back to NORMAL. Unknown modes or incomplete
responses never become an assumed normal setting.

A recovered backend acknowledgement means the requested mode was observed
after a lost native reply, not proof that this particular client caused it.
The native result preserves that distinction. No automatic retry is added.

## Native lifecycle and shared ownership

Collapsed panels make no setting requests. Visible settings use the existing
connection's daemon client, not another event stream or polling loop. Reads are
single-flight; obsolete requests must drain before the latest selection starts.
Backgrounding, leaving the panel, group changes and pending native commands
retire edit authority. Returning reads again. A draft is preserved only when
the re-read matches its original observation; otherwise it is discarded with
an explanation. Failed reads retain the original timestamp but disable editing.

Review selections are immutable and tied to the displayed setting and group.
Another slider edit or a new read retires old selections. Once handed to the
shared review, playback, schedules and undo cannot overlap the same command
session. Client preflight is not an atomic reservation against other apps;
backend policy and native recovery remain authoritative.

## Validation

Run `swift test --package-path apps/apple --filter ListeningSettings
-Xswiftc -warnings-as-errors` as one command. The contract and lifecycle suite
covers local edits, all six mode mappings, exact member scope, stale selection
rejection and read recovery. The command suite injects a controlled transport
into the actual DaemonClient and CommandReviewSession, exercising fresh reads,
exact payloads, shared exclusion, cancellation, one-use dispatch and uncertain
acknowledgements. These are synthetic daemon observations, not speaker tests.

The checked-in Xcode project includes ListeningSettingsPanel.swift. The
existing Native Apple remotes workflow builds both products, runs the full
package and launches the iPhone connection screen. That launch does not verify
the connected settings layout, VoiceOver interactions, tone audibility or
continuity of physical Sonos playback; those remain separate acceptance checks.
