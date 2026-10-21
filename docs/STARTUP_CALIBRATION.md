# Audible startup calibration followed by passive mpv synchronization

`stream --sync-probe <sink>` supplies an initial test signal to the existing
mpv synchronizer. This is a separate, explicit audible and microphone opt-in:
ordinary streaming, `--sync-mpv` alone and passive measurement do not emit it.
It works only with Linux PulseAudio or PipeWire's Pulse-compatible server,
`--sync-mpv`, and a named `--sync-microphone`. It is not an instantaneous chirp
or a physical lip-sync guarantee.

## Start once, then keep tracking the movie

Use an already configured virtual sink whose monitor carries mpv's audio,
and a distinct room microphone on the same local audio server. Start mpv
playing through that sink, with its IPC socket in a private directory. Replace
these illustrative names with your own:

```sh
fsonos --direct stream Kitchen --format pulse --input movie.monitor \
  --sync-mpv "$XDG_RUNTIME_DIR/fsonos-mpv.sock" \
  --sync-microphone room_mic --sync-probe movie
```

The sink must match the exact `<sink>.monitor` selected by `--input`. Defaults,
numeric device indices, another sink, non-Pulse capture, `--sync-input`,
`--measure-latency`, `--measure-probe` and `--latency-wav` are refused. Naming
checks are not proof that mpv is routed into that monitor: verify the actual
routing and use a virtual sink without competing local speaker output.
Everyone whose audio might be captured must know and agree to the capture.

Known paused or changed mpv playback is rejected in preflight before speaker
mutation. It is checked again when calibration starts. The command never
unpauses mpv, changes its output device, raises any volume, creates a sink or
changes audio-server defaults. The test signal can sound through local outputs
connected to the chosen sink as well as through Sonos.

## The handoff

The startup phase uses the same bounded noise, captured monitor reference,
PCM decoder, acoustic estimator and three-window agreement as
[active measurement](ACTIVE_CALIBRATION.md). It makes at most three attempts,
each using a 12–30 second capture, under a 180-second startup-phase budget.
Silence, ambiguous correlation or a failed capture cannot authorize a delay.
The signal is digitally limited, not acoustically loudness-calibrated.

All three captures must belong to one uninterrupted mpv playback generation.
During active capture, the existing IPC connection is checked before emission
and periodically while recording. A delivered pause, seek, restart, output or
media change ends the startup phase rather than refilling its audible budget.
The original Sonos source and group are checked before and after each window,
and once more after estimation. The final delay uses the existing absolute
`sync_base_delay - measured_lag` target and hysteresis, not cumulative offsets.

After successful calibration, no probe configuration remains in the passive
loop. Ordinary program audio supplies subsequent drift estimates; three fresh
windows are needed for any later correction. Pausing or seeking later cannot
restart the audible test signal. The original mpv delay is still restored by
the existing ownership-checked cleanup when the streaming session ends.

## Failure and cancellation

An inconclusive, interrupted or expired startup calibration stops synchronization
and reports the problem; ordinary audio streaming retains its existing lifetime
until stopped. There is no automatic new audible attempt or silent claim that
calibration succeeded. A lost mpv write reply is not replayed. Cleanup retains
the original media/output and delay ownership checks, including uncertain writes.

Capture exit, cancellation and failed playback checks stop and reap the probe
before the capture process. The deadline cancels capture through that path;
it does not preempt an IPC or speaker request already executing. Those requests
keep their own deadlines. Audio already buffered by Pulse or Sonos can still
sound afterward. Delivered events and read-before-write checks are not an atomic
transaction with another controller and cannot detect every transient change.

No microphone file is saved or uploaded. Real microphone routing, timestamp
accuracy, Sonos buffering and display timing still need installation-specific
qualification. This adds the startup-to-passive handoff, not automatic device
selection, macOS pairing, browser control or a faster single-chirp estimator.

## Regression checks

The startup Rust tests use generated delayed PCM and a real local Unix IPC
connection. They cover calibration, the handoff into the production passive
loop, restoration, silence, bounded attempts, pause/seek/reload, source rejection,
cancellation, deadlines, manual delay overrides and lost acknowledgements.
They do not connect to a real mpv process, Pulse server or speaker.

```sh
cargo test --locked -p fsonos-cli --bin fsonos streaming::synchronization
cargo test --locked -p fsonos-cli --bin fsonos \
  active_guard_failure_interrupts_a_real_capture_before_its_window_finishes -- --ignored
python3 scripts/test_stream_probe.py
```

The ignored test additionally runs the production capture lifecycle with a
local FFmpeg-generated silent source, not a microphone or audio output device.
The Python tests check the existing signal and MP3 path, not Rust compilation
or the startup control loop. Neither suite establishes real-room lip sync.
