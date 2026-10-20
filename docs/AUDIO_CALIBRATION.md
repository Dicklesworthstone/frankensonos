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

## Measure live latency without a video player

`--measure-latency` temporarily streams this computer's selected audio into the
room, measures its acoustic delay, then exits through the existing playback
restoration path. **It changes what the room plays and explicitly records the
selected microphone, but it never connects to or changes mpv.** Use ordinary
varied music or speech on the selected output; this mode does not play a
calibration chirp. Obtain consent from anyone whose audio may be captured.

For Linux PulseAudio (or PipeWire's PulseAudio-compatible server), substitute
actual stable source names for these illustrative names:

```sh
fsonos --direct --json stream Kitchen --format pulse --input movie.monitor \
  --measure-latency --sync-microphone room_mic --measure-timeout 180
```

With an existing paired stereo reference/microphone source instead:

```sh
fsonos --direct --json stream Kitchen --format pulse --input movie.monitor \
  --measure-latency --sync-input paired-calibration
```

The paired-source path also supports `alsa` or `avfoundation` with the same
routing requirements described below. Automatic pairing remains Linux/Pulse
only. Choose exactly one capture source and one action: `--measure-latency`
or `--sync-mpv`. Neither may accompany `--latency-wav`; `--sync-base-delay` is
for mpv correction only. Plain streaming still opens no measurement capture.

Each finite window uses the existing PCM decoder and acoustic estimator. Three
consecutive confident estimates must agree within 40 ms; the reported lag is
their median. Silence or ambiguous audio clears the pending agreement. Truncated
capture, failed speaker reads, changed source/group, or a paused/stopped stream
ends the measurement without an estimate. Initial buffering may precede a
window, but the speaker must report Playing afterward. Exact radio/HTTP aliases
are accepted; a different path, query, coordinator or group is not.

The default measurement-phase budget is 180 seconds; `--measure-timeout` accepts
10-600 seconds. An independent 64-window ceiling also applies. The deadline
cancels an active PCM capture through its kill/reap path. It does not include
initial discovery/playback setup or final restoration, and cannot interrupt an
already-running speaker request (that request retains its transport timeout).
Ctrl-C cancels the measurement and follows the same cleanup path.

On success, JSON is emitted only after capture is stopped and playback
restoration has been attempted. It contains `mode: "live"`, `lag_seconds`,
`lag_samples`, `sample_rate`, `agreeing_windows: 3`, `windows_read`,
`spread_samples`, `correlation_min` and `peak_ratio_min` (the lowest confidence
values among those three windows), and the suggested negative
`mpv_audio_delay_delta`. `mpv_modified` is always false. `playback_restored`
separately reports whether cleanup restored all requested playback aspects;
false is not disguised as a complete rollback. Failed measurement produces
an error rather than a stale or partial estimate.

As with ordinary streaming, newer owner playback/grouping is preserved by
read-before-write cleanup checks. These checks are not a transaction with
other controllers and cannot detect every change between observations.
Acoustic propagation, capture routing and microphone latency are part of the
measured lag. Agreement is not a physical lip-sync qualification or proof of
correct routing. No audio recording is written to disk or uploaded.

## Correct mpv while streaming

Choose either automatic Linux PulseAudio pairing or an already paired capture
source. For continuous correction, both require `--sync-mpv`; neither may be
combined with `--latency-wav` or with the other capture option. Without either option, no microphone capture
starts. Place the microphone near the listening position, with consent from
anyone whose audio would be captured.

Start mpv with a private local IPC socket, using a directory accessible only to
you. The examples assume your shell's `XDG_RUNTIME_DIR` is private; on macOS,
use a private directory of your own instead. Start the video before the stream:

```sh
mpv --input-ipc-server="$XDG_RUNTIME_DIR/fsonos-mpv.sock" movie.mkv
```

### Linux: pair a monitor and microphone automatically

With PulseAudio or PipeWire's PulseAudio-compatible server, list the available
sources using `pactl list short sources`. Select the exact named output monitor
that `stream` sends to Sonos and a distinct named microphone on the same local
server. For example, replacing these illustrative names with your own:

```sh
fsonos --direct stream Kitchen --format pulse --input movie.monitor \
  --sync-mpv "$XDG_RUNTIME_DIR/fsonos-mpv.sock" --sync-microphone room_mic
```

`--sync-microphone` removes the need to build a paired stereo source yourself.
It starts one measurement FFmpeg process with two Pulse inputs, keeps their
native latency-adjusted wall-clock timestamps, aligns their starting times,
resamples onto one 8000 Hz timeline, and maps reference left and microphone
right explicitly. It does not independently reset each input's first sample to
zero: that would mistake capture startup skew for acoustic delay. It requires
FFmpeg with `-isync` support (5.1 or newer).

Use stable source **names**, not numeric indices, `default`, `@DEFAULT_SOURCE@`
or `@DEFAULT_MONITOR@`. The reference must end in `.monitor`; the microphone
must not be another monitor. These checks happen before connecting to mpv or
changing the speakers. The option is supported only with Linux `--format pulse`;
macOS and direct ALSA capture must use a pre-paired source instead.

This pairs the selected sources; it does not discover your intended microphone,
load audio-server modules, create sinks, change defaults, or route mpv's audio.
Ensure the selected monitor actually carries the audio being streamed, and that
local loudspeakers are not giving the microphone a competing undelayed copy.
Both sources must use the same server's correctly reported timing. Hardware,
virtual-source or routing latency errors are not magically calibrated away:
a confident correlation does not prove correct microphone routing or lip sync.

### Linux or macOS: use a pre-paired source

Configure **one paired stereo capture source** in the local audio system.
Its left channel must be a dry reference of the output sent to Sonos; its right
channel must be the room microphone, aligned to that same clock. A generic
microphone, a normal stereo monitor, two independently zeroed capture devices,
or an uncalibrated aggregate device is not sufficient. Monitor/reference
latency introduced by this routing must be known.

```sh
fsonos --direct stream Kitchen --format pulse --input movie.monitor \
  --sync-mpv "$XDG_RUNTIME_DIR/fsonos-mpv.sock" --sync-input paired-calibration
```

`--input` is the audio sent to the speakers. `--sync-input` is the separate
paired reference/microphone source used only for measurement. Both use
`--format` (`pulse`, `alsa`, or `avfoundation`). This path does not create or
rewire the paired source and retains the previous single-input behavior.

### Measurement windows and correction

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

Continuous correction observes mpv's pause, speed, source, output and delay
properties on the same IPC connection. Delivered seeks, playback restarts and
audio reconfiguration invalidate the current capture and any earlier agreement.
A pause/resume cycle reported during a capture cannot turn that old window into
fresh evidence merely because mpv is playing again when capture ends. The loop
waits for three new uninterrupted windows before changing the delay. Events
arriving during estimation are checked again before the write.

A file reload (even the same path), a delivered source/output identity change,
or mpv's `event-queue-overflow` stops correction rather than carrying authority
into a different playback instance. Trailing and fragmented notifications are
processed under byte and time bounds. Property notifications can be coalesced
by mpv; this detects delivered events, not every possible transient change.

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

Both continuous correction and one-shot measurement verify the original Sonos
household, coordinator, room membership and stream URI before and after each
capture window. Continuous correction checks once more before applying a lag.
These reads use the existing stream supervisor and transport, not a second
connection or a fast polling loop. A different source/group, a paused/stopped
speaker, or a failed read stops correction and further measurement captures;
it does not send Play, rejoin rooms, or adopt the new source. A source change
inside a capture is noticed at the next window check, not instantaneously.

The correction worker waits at most 60 seconds for each supervisor verdict and
can abandon that wait on cancellation. Late verdicts cannot authorize a later
window. This does not interrupt a speaker request already executing on the
supervisor; its own transport timeout still applies. Source checks and the mpv
write are separate operations, so a concurrent owner change can still race them.
Ordinary streaming without microphone measurement does not add these reads.

Ctrl-C, encoder failure and normal shutdown stop the measurement worker, reap
the direct capture process, and attempt ownership-checked mpv cleanup before
the existing Sonos playback restoration. A synchronization failure is reported
immediately while ordinary audio streaming continues until stopped. After a
process crash or an unacknowledged cleanup, check mpv's `audio-delay` manually.

## Reproduce the hardware-free pairing checks

With Python 3.10+ and FFmpeg/ffprobe 5.1+ installed, run:

```sh
python3 scripts/test_stream_pairing.py
```

The six test groups use the **shipped** `pulse_pair.filter` with generated,
timestamped NUT inputs, not a microphone. They cover simultaneous and staggered
capture starts, mixed 44100/48000 Hz inputs, mono/stereo downmixing, timestamped
sample-clock drift, silence and short input. A negative control independently
zeros the two inputs and verifies that this changes the measured lag, rather
than allowing a broken fixture to pass. This checks FFmpeg processing, not Rust
compilation, CLI execution, or the acoustic estimator. Rust unit tests separately
cover opt-in, argument generation and source selection.

The live-measurement Rust regressions exercise generated PCM through the real
decoder, estimator and agreement collector, and source/group checks through
S1/S2 simulator sockets. Run the focused suites with the pinned Rust toolchain:

```sh
cargo test -p fsonos-cli --bin fsonos streaming::synchronization
cargo test -p fsonos-cli --bin fsonos streaming::recovery::measurement_tests
# Opt-in generated FFmpeg sources, no microphone or speakers:
cargo test -p fsonos-cli --bin fsonos real_ffmpeg_capture_feeds -- --ignored
cargo test -p fsonos-cli --bin fsonos deadline_interrupts_and_reaps -- --ignored
```

The continuous-correction regressions use real Unix socket pairs to exercise
IPC framing and event handling. Generated PCM passes through the production
loop, decoder and estimator to check successful correction, fresh agreement
after a seek, rejected room evidence and cancellation before a write. Separate
channel tests cover supervisor verdicts, deadlines, cancellation and late replies.
The socket peer and room-verification callback are controlled fixtures, not a
real mpv process or physical Sonos. These new Rust tests were source-reviewed
but not executed in the toolchain-less implementation environment.

The two ignored tests additionally exercise the production capture process:
one feeds generated delayed noise through the collector, the other checks
cancellation of a real-time silent source. `FFMPEG` may name an alternate local
executable. These tests are not physical Sonos, Pulse timing or mpv acceptance.
At implementation time, their standalone FFmpeg fixture commands were checked
locally (three 12-second windows retained a known 400 ms delay); the Rust test
binaries were not run because the local environment lacked a Rust toolchain.

Neither the Rust tests nor synthetic FFmpeg captures establish microphone
routing, Sonos buffering behavior, mpv display timing, or lip-sync accuracy on
a real installation. Automatic pairing outside Linux PulseAudio, automatic
device discovery, calibration chirps, browser/video-player integration beyond
mpv, and hardware acceptance remain separate work for issue #2.

Protocol references: mpv's official manual sections `--audio-delay` and
`JSON IPC`; FFmpeg's device documentation for Pulse `wallclock`, its `-isync`
input option, and the `aresample`/`join` audio filters. Pulse's native timestamps
include its latency reports; replacing them with packet-arrival wall time would
lose that information.
