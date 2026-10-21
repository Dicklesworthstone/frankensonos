# Stream selected applications through a temporary Linux output

`fsonos stream` can create a dedicated, temporary PulseAudio output instead of
requiring a manually configured virtual sink. This is an explicit local option:

```sh
fsonos --direct stream Kitchen --virtual-output movie
```

The command creates `movie`, displayed as `FrankenSonos_movie`, and captures
`movie.monitor`. Use your audio server's application-routing controls to send
only the intended application to that output. FrankenSonos does **not** move
applications, issue a default-output change, change any volume, or load a
loopback into local loudspeakers. A null sink does not itself play through local
speakers. Ordinary `stream --input ...` remains unchanged and runs no `pactl`.

Requirements: Linux, a local PulseAudio or PipeWire Pulse-compatible server,
`pactl` with JSON output support, and the existing FFmpeg/MP3 prerequisites.
`--format pulse` is the default and the only supported format for this option.
macOS and direct ALSA retain the existing explicit capture-device path.

Use a name of 1-64 letters, digits, hyphens or underscores, starting with a
letter. `default`, numeric indices, `.monitor` names and module-argument syntax
are rejected. `--virtual-output` cannot be combined with an explicit `--input`
or offline `--latency-wav`. An existing output or monitor is not reused,
reconfigured or removed: choose another name, or use the existing `--input`
path when you already manage that output yourself.

## Measurement and video synchronization

The chosen monitor is resolved before encoder, microphone-pairing and probe
arguments are built. The virtual output itself never starts microphone capture
or test noise; those still require their separate, explicit options:

```sh
fsonos --direct --json stream Kitchen --virtual-output movie \
  --measure-latency --sync-microphone room_mic \
  --measure-probe movie --measure-timeout 180
```

That command emits the bounded audible probe described in
[Active calibration](ACTIVE_CALIBRATION.md) and captures the named microphone.
Choose the intended source with the consent of people whose audio is captured.
For passive measurement omit `--measure-probe` and route suitable program audio
to the output. Continuous mpv synchronization also accepts `--virtual-output`
with the existing `--sync-mpv` and microphone options; its media/output identity
checks are unchanged. See [Acoustic calibration](AUDIO_CALIBRATION.md).

## Lifetime and ownership

After argument and speaker preflight, setup verifies a local Pulse connection
and checks both output and monitor names. It loads one `module-null-sink`, gives
it a random session ownership property, and reads back the output, module and
monitor relationship before starting capture. A failed, lost or malformed load
reply is never retried. Failed setup attempts ownership-checked cleanup and
never intentionally replaces the group's music.

The output stays available through the existing Sonos playback-restoration
attempt. Capture and probe processes stop before the output is removed. Normal
completion, Ctrl-C, SIGTERM and error unwinding use the same cleanup path. A
fresh read must identify this session's marker and module before an index-based
unload. A reused index, replacement output, duplicate ownership claim or failed
read cannot authorize removal. Read-back can confirm an unload whose reply was
lost; it never triggers a second unload. Cleanup failures are reported rather
than silently called success. Read-before-write is not an atomic transaction
with an audio server or another controller.

No permanent audio-server configuration is written. A process crash or SIGKILL
can leave the output loaded; inspect its ownership and current module using
`pactl list sinks` and `pactl list modules` before manually removing it. Do not
blindly reuse a module number printed by an earlier server session. PipeWire or
Pulse policy can choose where applications go when a device appears or
vanishes; route/stop applications deliberately before ending the stream. This
feature does not restore routing decisions made independently by the owner or
the audio server, and does not promise that a policy-selected default is fixed.

`pactl` is launched directly without a shell, with a five-second deadline and
one-MiB output limit per command. Cancellation applies to setup; cleanup has its
own bounded commands so Ctrl-C does not prevent removal. The executable is the
locally installed `pactl` on PATH, not a shell wrapper or remote service.

## Validation boundaries

Rust regressions cover argument resolution, creation/read-back, collisions,
module identity, lost replies, single-attempt cleanup and real child-process
output/deadline/cancellation handling. The protocol tests use synthetic responses;
the process tests run local process fixtures, not Pulse hardware. These tests do
not establish Rust compilation, physical audio routing, microphone timing or
Sonos lip sync unless the corresponding checks are actually executed.

Protocol sources: PulseAudio's `pactl.c` JSON/argument implementation and
`module-null-sink` documentation; PipeWire's `module-null-sink` documentation.
The JSON `monitor_source` field on source entries identifies the monitored sink
by name, not by numeric sink index.
