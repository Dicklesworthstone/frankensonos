# Acoustic latency measurement

The stream can measure a recording of its real acoustic path, without querying
or changing a speaker:

```sh
fsonos --direct --json stream Kitchen --latency-wav aligned.wav --max-lag-ms 8000
```

`Kitchen` labels the result; this mode does not discover or verify that room.
The recording must be **one time-aligned, two-channel capture**: left is the
computer audio sent to Sonos, right is the room microphone. Both channels must
share the same timeline. Two files started independently do not establish an
acoustic delay. Do not use a normal stereo music file as a calibration.

Use stereo signed 16-bit PCM WAV at 8000 Hz, 4-30 seconds long. The maximum
lag must be less than half the recording length; a 20-second recording works
with the default eight-second search. Existing aligned recordings can be
converted locally with FFmpeg:

```sh
ffmpeg -i aligned-original.wav -ar 8000 -ac 2 -c:a pcm_s16le aligned.wav
```

The command uses GCC-PHAT followed by a normalized-correlation check. Silence,
DC, unrelated audio, ambiguous competing peaks, and negative or out-of-range
delays fail instead of producing an actionable offset. Use varied speech or
music, not a repeating tone. Acoustic propagation and the microphone/capture
path are included in the result; microphone placement and routing matter.

JSON includes `lag_seconds`, `lag_samples`, `sample_rate`, `correlation`,
`peak_ratio`, and `mpv_audio_delay_delta`. A positive acoustic lag requires a
negative mpv `audio-delay` correction: positive mpv values delay audio, negative
values delay video. This mode reports the correction but does not apply it.
It reads a bounded local file and never stores or uploads microphone audio.

This is a measurement tool, not proof of hardware compatibility or a lip-sync
guarantee. The mathematical tests use generated audio; real-room qualification
is separate. Live drift correction is not enabled by this option.
