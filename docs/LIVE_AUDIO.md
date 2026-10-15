# Live computer audio

`fsonos stream` captures audio on the computer running the command and serves
live MP3 to a room's Sonos group. It needs a direct LAN route to the speakers
and a locally installed FFmpeg with the `libmp3lame` encoder. It does not use
the daemon or expose the control API through the audio listener.

```sh
# Linux: PipeWire/PulseAudio's default output monitor.
fsonos stream Kitchen

# Select a particular output monitor and bitrate.
fsonos stream Kitchen --input sink.monitor --bitrate 256

# macOS: supply the appropriate AVFoundation audio device index.
fsonos stream Kitchen --format avfoundation --input :0
```

## Startup and reconnects

The command snapshots the original group, binds a random read-only URL on the
speaker-facing LAN interface, and starts capture. It waits up to ten seconds
for a complete encoded MP3 frame before replacing the music. A silent,
misconfigured, failed or prematurely exited encoder therefore fails before a
playback write when detected during preparation. Source, group, transport
and queue identity are checked again after capture preparation, followed by
another encoder/cancellation check immediately before dispatch. Newer
playback is not deliberately displaced by a stale startup snapshot.

Capture is drained continuously, including while no speaker is connected.
HTTP clients have independent cursors into a bounded window of 128 complete
MPEG-1 frames, rather than sharing a blocking copy of the encoder pipe. A new
connection starts at the latest complete frame. The encoder disables its bit
reservoir and file preambles so a reconnect does not require the old client's
audio prefix. A reader falling behind the retained window disconnects instead
of holding back capture or other readers.

The listener accepts only GET and HEAD at the generated URL. Client count,
header size, header time and write time are bounded. A partial request or a
reset connection does not terminate the whole stream.

These limits bound application buffering, not Sonos's internal buffering or
all operating-system network buffers. **Ordinary streaming does not synchronize
video.** An experimental, explicit `--sync-mpv` / `--sync-input` pair now adds
acoustic feedback and guarded mpv delay correction. `--latency-wav` measures
an existing synchronized reference/microphone recording without contacting a
speaker. See [Acoustic calibration](AUDIO_CALIBRATION.md) for required channel
routing, ownership safeguards and limitations. Automatic source pairing,
startup chirps and real-hardware lip-sync acceptance remain work for issue #2;
no fixed end-to-end latency or lip-sync guarantee is claimed.

## Stopping and restoration

Ctrl-C requests shutdown without first closing the audio connections. While
capture is still available, cleanup checks the captured coordinator's current
media URI and group membership. It addresses that coordinator by identity,
not by a room name that could have changed.

Only the source, queue position and transport are candidates for restoration.
Volume, mute and grouping are not restored: streaming did not own those
settings, and later adjustments must survive. A source switch, regroup, queue
revision change or unreadable ownership check prevents dependent restoration
writes. Source/group ownership and the queue revision are checked again before
each playback restoration step. A paused or stopped, previously acknowledged
stream does not cause the old source to be automatically resumed.

Cleanup also runs after an attempted Play fails: a failed reply does not prove
that the stream URI was not installed. It restores only when read-back still
identifies this session's source. The encoder is then killed and reaped, and
its capture and HTTP workers are joined, including on partial setup failures.

These are bounded read-before-write safeguards, not a transaction with other
Sonos clients. A competing write can still occur between a check and its SOAP
operation. Skipped or failed restoration is reported rather than presented as
a complete rollback. Real-speaker compatibility and latency still require
hardware qualification.
