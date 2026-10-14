# Rooms, groups, and moving music in the web remote

Open **Rooms & groups** on the daemon's web remote. Nothing is read until
this panel opens. **Refresh rooms** reads the current group view; rooms are
listed with both their household and their current group so repeated room
names in S1 and S2 do not select a household by accident.

Choose **Room to change** and, for a join or music transfer, a
**Destination room / group**. Every action first opens a review, then needs
**Confirm group change**. Cancel sends no mutation. Changing either room,
refreshing, or closing the panel discards the review.

## Join, separate, and group a household

**Review join** moves the selected room into the destination room's group.
The joining room takes that group's music. This is not a move of all rooms
in the source group. Moving a coordinator can affect the rooms left behind;
the review lists both affected groups and warns about playback changes.

**Review separation** takes the selected room out of its current group.
It never sends an all-rooms wildcard. A standalone room needs no change.

**Review whole household** groups every observed room of the source room's
household under its existing group's coordinator. It does not include the
other household. The review lists every affected group; a newly discovered
room invalidates that review rather than being silently included.

S1 and S2 rooms cannot share a group. Cross-household joins are refused by
the panel and the existing API. Already-satisfied group actions send nothing.

## Move music, or replay it in another household

**Review music move** uses the existing same-household move action. The
destination room joins the source music, then the selected source room leaves.
A coordinator handoff carries the playing queue; the controller can use its
existing replay fallback when delegation is unavailable. Other rooms in the
source group can keep playing. A move between two rooms already in one group
can still hand off the coordinator or remove the selected source member.

**Review replay elsewhere** invokes the API's `copy: true` operation. It
replays the source group's current source or queue on the **destination group**
and then stops the **source group**. It can replace the destination queue.
Despite the API field's name, this is not a command to keep both groups
playing. It supports the existing cross-household replay path, subject to
the destination's media support and Spotify linkage; it does not synchronize
playback between households. Replaying onto the very same group is a no-op.

The review shows both groups' current track and transport state. Empty or
unreadable sources cannot be moved through the panel. A paused source can be
reviewed; the existing move/replay action determines how it is restored.

## Checks, results, and limits

The panel re-reads the affected group memberships immediately before sending
one existing `/group`, `/ungroup`, `/party`, or `/move` request. Transfers also
re-read the source and destination playback, comparing track identity,
transport state, and queue position when reported. Natural elapsed-position
and display-title changes do not invalidate the review. Group or track
changes that are observed do invalidate it.

These are client-side checks over the daemon's live/cached API view, not an
atomic lock on the speakers. A competing controller can act after a check;
identical room names reused after a rename or a track restarted with the same
reported identity are not a device-epoch guarantee. Queue contents beyond the
current entry are not frozen by a transfer review. The existing controller
remains responsible for native orchestration, policy, volume caps, and logging.

Only one panel operation runs at a time. A lost, malformed, or failed mutation
response is never automatically repeated or converted into a different kind
of move. The panel reports possible partial effects and requires a fresh read
before another action. Closing during preflight prevents dispatch; closing
after dispatch cannot withdraw an operation already sent to the daemon.

Policy notes and partial-result descriptions remain visible. Existing undo
is best-effort playback restoration through the action log, not a guaranteed
rollback of queue contents or concurrent changes. Successful actions refresh
the panel and room cards without replaying the action.

## Tests

`node --test scripts/test-remote-groups.cjs scripts/test-remote-transfers.cjs`
executes the shipped client model. The Linux workflow runs these alongside
the queue and library client regressions. Offline browser fixtures with
synthetic replies check the DOM, controls, request serialization, and layout;
they are not evidence of Rust, networked-daemon, or physical-Sonos integration.
