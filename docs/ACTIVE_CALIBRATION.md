# Measure acoustic lag without program audio

`stream --measure-latency` can optionally supply its own test signal instead of
waiting for matching music or speech. The new **`--measure-probe <sink>`** is an
explicit audible opt-in, only for Linux PulseAudio or PipeWire's Pulse-compatible
server, and only for measurement-only mode with `--sync-microphone`.

It does not connect to mpv, modify the account, adjust any volume, create a
virtual sink, change audio-server defaults, or alter ordinary streaming. A
physical output connected to the selected sink may also sound. Use a dedicated
virtual sink with no local acoustic output when measuring the Sonos path, to
avoid feeding an undelayed local copy into the microphone. Everyone whose audio
might be captured should know and agree to the measurement.

For an initial calibration that actually corrects mpv and then continues passive
tracking, use the separate [startup synchronization opt-in](STARTUP_CALIBRATION.md),
`--sync-probe`. `--measure-probe` remains measurement-only and never changes mpv.

## Select the output and microphone explicitly

List the available sinks and sources with `pactl list short sinks` and
`pactl list short sources`. For example, after replacing these illustrative
names with your actual, already configured devices:

```sh
fsonos --direct --json stream Kitchen \
  --format pulse --input movie.monitor \
  --measure-latency --sync-microphone room_mic \
  --measure-probe movie --measure-timeout 180
```

The probe sink must be exactly the name before `.monitor` in `--input`.
Defaults, numeric indices, a different sink, and monitor names used as sinks
are rejected. This is a conservative naming contract, **not device discovery
or verification of custom audio-server routing**. The monitor and microphone
still need correctly reported, shared-server timestamps. Check the selected
names and routing yourself; the implementation does not infer intent from a
correlation score.

`--measure-probe` cannot be used with `--sync-mpv`, `--sync-input`, or
`--latency-wav`. Remove it to retain passive measurement. Non-Linux platforms
and other capture formats retain their existing passive paired-input path.

## What is emitted

The probe is broadband test noise, not a repeating pure tone and not an
instant startup chirp. Its generated signal is band-shaped at 250–3000 Hz,
with every sample clipped to ±0.025 before stereo output conversion and a
short fade-in and final fade-out. The digital ceiling is not a calibrated
sound-pressure limit: speaker, room and Pulse sink volumes still determine
how loud the result is. No automatic gain increase is attempted after a
failed measurement.

There are at most **three probe windows**, each bounded to the capture length
of 12–30 seconds. The existing measurement-phase deadline can end them sooner.
Every window begins a new random noise sequence. The probe starts only after
the measurement capture supplies its first complete stereo PCM frame, and a
capture that has already exited cannot authorize it. Failed input startup
therefore does not intentionally emit a signal.

The reference is still recorded from the actual monitor. It is not replaced
with the generated samples, and capture startup time is not used as a presumed
acoustic emission time. The existing estimator and three-window agreement
requirement are unchanged. If three windows cannot establish agreement, the
measurement fails instead of generating an indefinite audible retry loop.

Capture cancellation, failure and normal completion stop and reap the direct
probe process before stopping the capture process. The probe's own FFmpeg
output also has a finite duration. This does not remove audio already buffered
by Pulse or Sonos instantly. The existing ownership-checked playback restoration
runs when measurement ends and reports `playback_restored` separately.

## Verification and limits

Run the hardware-free signal tests with Python 3.10+ and a local FFmpeg that
includes the filters and MP3 encoder already needed for streaming:

```sh
python3 scripts/test_stream_probe.py
```

The seven tests use the shipped `probe.lavfi`, not an independent signal
implementation. They check exact finite lengths, the digital amplitude bound,
stereo identity, fades, independent windows, DC offset, the actual stream's
MP3 settings, and termination of a real-time FFmpeg fixture. The codec test
uses an independent sub-sample correlation oracle to distinguish codec delay
from a separately injected 400 ms delay. No test connects to Pulse or records
a microphone.

Rust tests cover CLI opt-in, named-source validation, the three-window budget,
argument construction, and the first-frame readiness boundary. An explicitly
ignored FFmpeg test additionally feeds generated probe audio through the
production Rust capture, decoder, estimator and measurement collector:

```sh
cargo test -p fsonos-cli generated_probe_passes_the_production_capture_and_measurement_path -- --ignored
```

The Python signal tests are not proof that Rust builds, CLI parsing works,
Pulse routes the chosen devices correctly, or physical lip sync is acceptable.
Real-room Sonos/Pulse qualification, macOS pairing, automatic device selection
and a faster single-startup-chirp workflow remain separate work.

Implementation references: FFmpeg's official Devices documentation (Pulse
output `device` and `buffer_duration`) and Filters documentation (`anoisesrc`,
`highpass`, `lowpass`, `aeval`, `afade`).
