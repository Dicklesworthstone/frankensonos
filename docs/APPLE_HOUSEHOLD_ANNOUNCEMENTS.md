# Announce across groups in the native remote

Select a group, then open **House → Announcements**. The recipient picker lists
all observed groups in that selected household. Choose individual rooms from
one or several groups for speech, a chime, or an already validated WAV.
No room is selected by default. Editing the composer does not send a request.

**Select this group's rooms** still means only the selected group. The separate
**Select all N rooms in [household]** shortcut explicitly selects that observed
household; it never includes the other household or sends the daemon's implicit
`all` target. At most 256 rooms can be selected. An over-limit select-all fails
without truncating the list or changing the earlier selection.

Review shows the exact speech or retained WAV details, qualified room names,
requested level and affected group count. Confirming uses one existing
`POST /announce`, not one request per group. Policy caps, clip generation,
serialization, temporary grouping and restoration remain on the daemon.
A native announcement still addresses one household, not both S1 and S2 at once.

## What the client checks

The command retains every recipient's original complete group, including its
coordinator. Preparation and confirmation reread all members of those groups,
including members not receiving the clip, because temporary grouping/restoration
can affect their playback and levels. A changed or unreadable affected member
stops the whole command before dispatch. Unrelated groups and the other household
do not become state-read dependencies. The selected navigation group remains a
preflight dependency even when only rooms in another group are selected.

The final client topology read also checks all retained recipient groups.
Normal position-clock progress and member ordering do not invalidate the review;
changed music, mute, levels or group membership do. These reads are not an atomic
reservation against other controllers. The daemon owns the final operation and
reports failures to restore previous state.

A changed household layout clears the composer's recipients and cancels pending
file imports while retaining typed text and already loaded audio. Closing the
composer, hiding it, disconnecting or backgrounding clears recipients. Recipients
never silently expand as new rooms appear. An issued immutable review does not
follow later changes to the composer or sidebar.

The response must cover every selected room. Finishing the clip without restoring
the music is reported as partial, not successful restoration. Lost, incomplete or
contradictory acknowledgements stay uncertain and are never automatically repeated.
Announcements are not undoable. Cancelling or closing the app cannot recall an
already sent announcement.

## Verification boundary

`HouseholdAnnouncementTests.swift` covers explicit same-household payloads for S1
and S2, all three clip sources, unchanged single-group API behavior, byte-exact
identities, topology drift, all affected member reads, recipient limits, file
import lifecycle, cancellation, one-use command ownership and partial/uncertain
receipts. Existing `AnnouncementFormTests.swift` remains unchanged.

The focused Swift 6.2.1/Linux harness runs 20 new test functions plus six existing
composer functions with warnings treated as errors. It includes the exact changed
production command/helper files, the real playback-review actor and observation
code, and production snapshot/transport/read/write paths; unrelated command and
store dependencies are isolated. A synthetic localhost HTTP fixture also exercises
the production URLSession adapter and shared review, not a running Rust daemon
or physical speakers. Three integration regressions fail when the two new command
hooks are removed and pass when restored.

The complete changed view passes syntax parsing for iOS 17 and macOS 14. This is
not Apple SDK type-checking, a full native-package run or rendered UI validation.
Live-daemon integration and physical Sonos acceptance remain separate checks.
