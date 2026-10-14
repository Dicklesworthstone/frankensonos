# Announcements from the web remote

Open **Announcements** in the daemon's phone remote, choose individual rooms,
and select **Speak text**, **Play a chime**, or **Upload a WAV clip**. No rooms
are selected for you.
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

## Upload a recorded clip

Select **Upload a WAV clip** and choose a file on the phone or computer running
the browser. Choosing it only reads the local file. It is not uploaded until
**Review announcement**, followed by **Announce now**, and the group recheck.
The review shows the filename, duration, channel count, sample rate, byte size,
rooms and requested volume. The filename is display-only, never a daemon path.

The browser checks the complete RIFF container: declared length, chunk bounds
and padding, format before data, duplicate format/data chunks, nonempty aligned
samples, and consistent byte rate. It accepts mono/stereo 16-bit PCM, including
PCM wrapped in WAVE_FORMAT_EXTENSIBLE, at the same nine rates as the daemon:
8,000; 11,025; 12,000; 16,000; 22,050; 24,000; 32,000; 44,100; and 48,000 Hz.
The complete file, including metadata, must fit 16 MiB; audio may last no more
than five minutes. Compressed, float, unsupported-rate and incomplete WAVs are
rejected, not transcoded. A `.wav` extension or MIME type alone is insufficient.

After review, the unchanged file bytes (including metadata chunks) are encoded
as standard padded base64 in the existing `wav_base64` field. The daemon checks
them again independently before playback. No filename, local filesystem path,
voice, chime or speech field is sent with a WAV upload. The browser's check is
an early usability check, not a substitute for server validation.

A file read has a fifteen-second deadline. Replacing or clearing a selection,
refreshing rooms, closing or disposing the panel cancels its current reader;
late completion cannot restore obsolete bytes. Changing source or closing the
panel discards the loaded clip. Dispatch consumes it, so another WAV announcement
requires choosing the file again. No microphone is accessed and no audio is
stored in browser persistence. The daemon retains uploaded clips according to
its existing media-store lifetime. All announcement sources share the same
review, policy, restoration and uncertain-outcome handling.

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
controller dispatch, not the Rust daemon. WAV cases include all 18 supported
rate/channel pairs, extensible PCM, malformed chunks, exact size/duration
boundaries, cancelled readers, and byte-for-byte base64 round trips. The existing Rust announcement
simulator tests remain the backend integration layer. The component is
embedded in the existing script and stylesheet; browser policy is unchanged.
Physical-speaker acceptance, speech-engine setup on the deployed daemon, and
full browser-to-daemon qualification remain separate from these checks.
