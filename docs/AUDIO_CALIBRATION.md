# Acoustic latency measurement and experimental mpv synchronization

Ordinary `fsonos stream` does not record a microphone or change a video player.
The options below are explicit, local-only opt-ins on Linux and macOS. They
add no cloud service, microphone upload, speaker firmware change or remote
microphone endpoint. Real-room qualification is still required; this is not a
fixed-latency or lip-sync guarantee.

## Measure an existing recording

```sh
fsonos --direct --json stream Kitchen --latency-wav aligned.wav --max-lag-ms 8000
```

`Kitchen` labels the result; this mode does not discover or verify that room.
The recording must be **one time-aligned, two-channel capture**: left is the
computer audio sent to Sonos, right is the room microphone. Both channels must
share the same timeline. Two files started independently do not establish an
acoustic delay. Do not use a normal stereo music file as a calibration.

Use stereo signed 16-bit PCM WAV at 8000 Hz, 4-30 seconds long. The maximum
lag must fit within half the recording length with headroom at the boundary;
a 20-second recording works with the default eight-second search. Existing
aligned recordings can be converted locally:

```sh
ffmpeg -i aligned-original.wav -ar 8000 -ac 2 -c:a pcm_s16le aligned.wav
```

The command uses GCC-PHAT followed by normalized correlation. Silence, DC,
unrelated audio, ambiguous competing peaks, and negative or out-of-range
delays fail instead of producing an actionable offset. Use varied speech or
music, not a repeating tone. Acoustic propagation and the microphone/capture
path are included in the result; microphone placement and routing matter.

JSON includes `lag_seconds`, `lag_samples`, `sample_rate`, `correlation`,
`peak_ratio`, and `mpv_audio_delay_delta`. A positive acoustic lag requires a
negative mpv `audio-delay` correction: positive mpv values delay audio, negative
values delay video. This mode reports the correction but does not apply it.
It reads a bounded local file and never stores or uploads microphone audio.

## Correct mpv while streaming

First configure **one paired stereo capture source** in the local audio system.
Its left channel must be a dry reference of the output sent to Sonos; its right
channel must be the room microphone, aligned to that same clock. A generic
microphone, a normal stereo monitor, two independently started capture devices,
or an uncalibrated aggregate device is not sufficient. The command does not
create or rewire this source for you. Monitor/reference latency introduced by
the routing must be known; a confident correlation is not proof of correct
routing. Place the microphone near the listening position, with consent from
anyone whose audio would be captured.

Start mpv with a private local IPC socket, using a directory accessible only to
you. The example assumes your shell's `XDG_RUNTIME_DIR` is private; on macOS,
use a private directory of your own instead. Start the video before the stream:

```sh
mpv --input-ipc-server="$XDG_RUNTIME_DIR/fsonos-mpv.sock" movie.mkv

fsonos --direct stream Kitchen --format pulse --input movie.monitor \
  --sync-mpv "$XDG_RUNTIME_DIR/fsonos-mpv.sock" --sync-input paired-calibration
```

`--input` is the audio to send to the speakers. `--sync-input` is the separate
paired reference/microphone source used only for measurement. Both use
`--format` (`pulse`, `alsa`, or `avfoundation`). Neither synchronization option
is valid alone or together with `--latency-wav`. Without them, the old streaming
path remains unchanged and no microphone capture starts.

The synchronizer reads mpv's current state before any speaker mutation, then
starts recording only after stream playback succeeds. It captures finite PCM
windows into bounded memory, without audio files. Each window is 12-30 seconds,
chosen to accommodate `--max-lag-ms` (20 seconds at the default). Failed or
truncated captures stop synchronization, and their process is killed/reaped.
Only the explicitly selected, locally installed FFmpeg executable is launched;
arguments are passed without a shell.

Three consecutive, confident measurements must agree within 40 milliseconds.
Their median is used, and an adjustment is sent only when it differs from the
last owned delay by more than 40 milliseconds. Silence, ambiguous audio and
pause hold the last good setting, clear pending confidence, and require fresh
agreement. A correction also clears prior measurement confidence. This is
conservative drift correction, not a startup chirp or an immediate calibration.

The target is **`sync_base_delay - measured_lag`**, never the current correction
minus another measured lag. `--sync-base-delay` defaults to zero. Set it only
for an intentional, uncorrected media-specific offset, for example
`--sync-base-delay 0.2`. Repeated measurements and restarting a session therefore
do not accumulate offsets. The value mpv had at startup is recorded separately
for cleanup, not used as the correction baseline.

## Ownership, errors and stopping

The synchronizer stays on one mpv connection. It does not launch or reconnect
to a replacement player. Commands have request IDs, bounded response/event
bytes, and total read/write deadlines; malformed, denied or lost replies are
not replayed. The initial local socket connection happens in preflight.

A media/output switch, non-normal playback speed, or a changed `audio-delay`
causes correction to stop. Paused playback is never resumed. On shutdown, the
old delay is restored only if the same media/output still has this session's
last attempted or confirmed value. An uncertain write can therefore be read
back for cleanup without replaying it. An external setting is not deliberately
overwritten. mpv read-before-write checks are not an atomic transaction with
other controllers; a concurrent change between a check and a write remains
possible. Failed or skipped cleanup is reported, never called a full rollback.

Ctrl-C, encoder failure and normal shutdown stop the measurement worker, reap
the direct capture process, and attempt ownership-checked mpv cleanup before
the existing Sonos playback restoration. A synchronization failure is reported
immediately while ordinary audio streaming continues until stopped. After a
process crash or an unacknowledged cleanup, check mpv's `audio-delay` manually.

The Rust regressions use generated audio and local Unix socket fixtures, not
physical speakers. Neither the tests nor synthetic FFmpeg captures establish
microphone routing, Sonos buffering behavior, mpv display timing, or lip-sync
accuracy on a real installation. Automatic source pairing, calibration chirps,
browser/video-player integration beyond mpv, and hardware acceptance remain
separate work for issue #2.

Protocol references: mpv's official manual sections `--audio-delay` and
`JSON IPC`, and FFmpeg's official device documentation for the selected input.
