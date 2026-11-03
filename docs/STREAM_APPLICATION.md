# Launch a foreground application into a temporary streaming output

On Linux with a local PulseAudio or PipeWire Pulse-compatible server, the
`stream` command can launch a new foreground application with its Pulse output
preference set to the session's temporary output:

```sh
fsonos --direct stream Kitchen --virtual-output movie -- mpv --ao=pulse movie.mkv
```

Everything after `--` is the executable and its literal argument array, not a
shell command string. Quotes are handled by your invoking shell as usual;
FrankenSonos does not expand variables, interpolate arguments, or run a shell
implicitly. Choose a trusted, locally installed application. Its working
directory, environment and privileges otherwise remain yours; this is not a
sandbox. Application arguments are not added to the Sonos action log.

The temporary sink is created and verified, capture is warmed up, and Sonos
must accept playback before the application is launched. The new launch path
then waits up to ten seconds for a live HTTP GET handler to write a complete
MP3 frame. HEAD requests, response headers alone, and disconnected readers do
not authorize launch. It rechecks the original Sonos group and stream source
(playing or initially buffering), then verifies the reader is still active.
A missing reader, changed group/source, cancellation, or failed read prevents
launch and goes through cleanup. A denied or failed play request never launches
it. Spawn failure goes through the existing ownership-checked playback
restoration and stream/output cleanup. No automatic
application restart occurs after a failure or exit. The reader check avoids
starting the app before the first media connection, but it is not an atomic
transaction with a remote client and does not prove acoustic playback. A later
disconnect or an unrelated client fetching the random stream URL cannot be
ruled out by successful socket writes.

## Routing is a child-local preference

Only the child receives `PULSE_SINK=movie`. The parent environment and system
output default are unchanged; no existing application is moved. The program
must use Pulse and honor that preference. An explicit application-selected
output overrides it. A single-instance app may hand the request to an already
running process, which does not inherit this environment. Such applications,
background launchers, daemonizing programs, and native non-Pulse outputs are
not supported by this foreground lifecycle. Use the manual routing described
in [Virtual outputs](VIRTUAL_OUTPUT.md) for applications already running.

Child stdin and stdout are disconnected. This keeps the controller's terminal
input and JSON output separate from the application's output. Child stderr
remains visible. Use the application's GUI or another explicit control channel
rather than interactive terminal input.

This mode requires `--virtual-output`. It is separate from `--latency-wav`,
`--measure-latency`, and correction of an already running player through
`--sync-mpv`. It does not start a microphone, emit a probe, or correct video
latency. Ordinary `stream` and all existing calibration paths are unchanged.

## Finishing and cleanup

Close the app normally or stop it with Ctrl-C in the streaming terminal.
A natural application exit is reported immediately, but the stream remains
available until Ctrl-C: Sonos can still have buffered audio, and the controller
cannot prove that the last sample has been heard. There is no guessed drain
time, source replacement on app exit, or automatic restart. An error exit is
reported immediately and returned after the streaming session is stopped.

Ctrl-C, SIGTERM, stream failure and ordinary unwinding stop and reap the owned
direct child before speaker restoration and temporary-output removal. This is
not process-tree management: child-created background processes are not owned.
Do not use a shell pipeline or a launcher that delegates playback elsewhere.
A still-running direct child is forcibly terminated during cleanup, so save
any application state before cancelling. SIGKILL or a controller crash cannot
perform this cleanup. If direct-child cleanup cannot be confirmed, the output
is deliberately left loaded and a warning requests manual inspection, rather
than removing its destination and provoking an audio-server fallback.

Output disappearance, routing policies and competing applications are not an
atomic transaction. The `PULSE_SINK` preference is not proof that the intended
program is what the microphone or speakers hear. Physical routing and Sonos
playback still require installation-specific verification.

The Rust tests include command-line and command-construction cases plus real
self-spawned process fixtures. They use no audio server, microphone or speaker.
Adding these tests does not establish that they have been executed; check the
reported Rust/CI results separately. Protocol reference: PulseAudio's official
FAQ, "What environment variables does PulseAudio care about?", documents that
an application's explicitly selected sink takes precedence over `PULSE_SINK`.
