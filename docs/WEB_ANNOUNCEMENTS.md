# Announcements from the web remote

Open **Announcements** in the daemon's phone remote, choose individual rooms,
and select **Speak text** or **Play a chime**. No rooms are selected for you.
Identical room names in different households have separate qualified targets.
The panel never sends an empty room list, which the underlying API would
otherwise interpret as the whole house.

Speech accepts up to 1,000 Unicode characters. The optional voice belongs to
the local speech engine configured on the daemon host; leaving it blank uses
that engine's configured default. The browser does not synthesize speech,
record a microphone, or contact a third-party speech service. The three
built-in chimes are `bell`, `beep`, and `rise`.

Choose a requested volume from 0 to 100 (initially 35). The existing house
policy still caps each room. **Review announcement** shows the exact content,
selected rooms, affected groups, and requested volume. **Announce now** sends
one `POST /announce`; **Cancel announcement** sends nothing. Changing a field
or selection invalidates the review. Before sending, the panel rereads groups
and rejects a changed selected group. These observations are not an atomic
lock against other controllers or changes during synthesis.

## Interruption and restoration

The daemon makes the clip, temporarily groups selected rooms, applies capped
levels, plays it, and attempts to restore the previous state. Selecting a
room can affect its other group members even when they were not selected.
In particular, the core announcer documents that selecting a coordinator but
not its whole group can rewind the other members when restoring the snapshot.
Review the affected groups, not just the destination room names.

The request waits through generation, any household queueing, playback, and
restoration. Its result shows the applied room levels, each household's
playback outcome, and every reported failure to restore state. A partial
result is not displayed as clean success. Result room names come from the
daemon's display names, whereas destination selection is household-qualified.
Announcements restore themselves; the panel offers no separate undo.

Closing during the initial group check prevents an unsent request. Closing
AFTER dispatch does not stop the announcement or issue a second request. The
browser retains the in-flight request and its result while this page exists.
Navigating away can lose the result without stopping the daemon's work.

A lost or unreadable reply is uncertain: the daemon may still be synthesizing,
playing, or restoring. There is no automatic retry, local fallback, or claimed
cancellation. Check playback, then refresh rooms before reviewing another
announcement. A fresh group read does not prove an earlier announcement ended.
Draft text and room choices stay only in the page's memory, not browser storage.

## Verification

`node --test scripts/test-remote-announcements.cjs` executes the shipped model
with controlled replies and a real loopback JSON endpoint. The latter tests
controller dispatch, not the Rust daemon. The existing Rust announcement
simulator tests remain the backend integration layer. The component is
embedded in the existing script and stylesheet; browser policy is unchanged.
Physical-speaker acceptance, speech-engine setup on the deployed daemon, and
full browser-to-daemon qualification remain separate from these checks.
