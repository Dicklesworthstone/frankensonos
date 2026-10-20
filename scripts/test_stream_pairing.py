#!/usr/bin/env python3
"""Real FFmpeg tests of the shipped pairing filter; no devices or network.

Run with Python 3.10+ and FFmpeg/ffprobe 5.1+ installed. Generated NUT inputs
stand in for Pulse's shared-clock, latency-adjusted timestamps. This checks
FFmpeg alignment and channel mapping, NOT Rust execution, Pulse hardware
latency reports, acoustic estimation, or mpv/Sonos acceptance.
"""
from __future__ import annotations

import array
import json
import math
import os
from pathlib import Path
import random
import shutil
import subprocess
import sys
import tempfile
import unittest
import wave

ROOT = Path(__file__).resolve().parents[1]
FILTER = (ROOT / "crates/fsonos-cli/src/streaming/synchronization/pulse_pair.filter").read_text().strip()
FFMPEG = os.environ.get("FFMPEG", "ffmpeg")
FFPROBE = os.environ.get("FFPROBE", "ffprobe")
RATE = 8_000
WINDOW = 4
EPOCH = 1_000.0


def command(executable: str, *args: str) -> bytes:
    result = subprocess.run(
        [executable, *args], stdin=subprocess.DEVNULL, capture_output=True,
        check=False, timeout=20,
    )
    if result.returncode:
        raise AssertionError(f"{executable} failed: {result.stderr.decode(errors='replace')}")
    return result.stdout


def ffmpeg(*args: str) -> bytes:
    return command(FFMPEG, "-hide_banner", "-loglevel", "error", "-nostdin", "-y", *args)


def pcm_bytes(samples: list[int]) -> bytes:
    data = array.array("h", samples)
    if sys.byteorder != "little":
        data.byteswap()
    return data.tobytes()


def stereo(data: bytes) -> tuple[list[int], list[int]]:
    if len(data) % 4:
        raise AssertionError("partial stereo sample")
    samples = array.array("h")
    samples.frombytes(data)
    if sys.byteorder != "little":
        samples.byteswap()
    return list(samples[::2]), list(samples[1::2])


def best_lag(left: list[int], right: list[int]) -> tuple[int, float]:
    # An independent sparse normalized correlation oracle. Smoothing the
    # generated noise gives a peak wide enough for coarse then sample refinement.
    anchors = list(range(2 * RATE, 3 * RATE, 41))
    x = [left[i] for i in anchors]
    mean = sum(x) / len(x)
    x = [v - mean for v in x]
    energy = sum(v * v for v in x)

    def score(lag: int) -> float:
        y = [right[i + lag] for i in anchors]
        mean_y = sum(y) / len(y)
        y = [v - mean_y for v in y]
        denominator = math.sqrt(energy * sum(v * v for v in y))
        return sum(a * b for a, b in zip(x, y)) / denominator if denominator else 0.0

    coarse = max(range(-7_200, 7_201, 8), key=score)
    lag = max(range(coarse - 8, coarse + 9), key=score)
    return lag, score(lag)


class PairingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        rng = random.Random(0xF50A)
        raw = [rng.randrange(-10_000, 10_001) for _ in range(9 * RATE + 16)]
        cls.signal = [sum(raw[i:i + 12]) // 4 for i in range(9 * RATE)]

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="fsonos-pair-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.serial = 0

    def value(self, time: float) -> int:
        sample = time * RATE
        index = math.floor(sample)
        if not 0 <= index < len(self.signal) - 1:
            return 0
        fraction = sample - index
        return round(self.signal[index] * (1 - fraction) + self.signal[index + 1] * fraction)

    def media(
        self, start: float, lag: float = 0.0, *, rate: int = RATE,
        channels: int = 1, duration: float = 6.0, silence: bool = False,
        speed: float = 1.0,
    ) -> Path:
        self.serial += 1
        path = self.root / f"input-{self.serial}.wav"
        samples = []
        for n in range(round(duration * rate)):
            value = 0 if silence else self.value(start + n / rate * speed - lag)
            samples.append(value)
            if channels == 2:
                samples.append(round(value * 0.75))
        with wave.open(str(path), "wb") as output:
            output.setparams((channels, 2, rate, 0, "NONE", "not compressed"))
            output.writeframes(pcm_bytes(samples))
        target = path.with_suffix(".nut")
        ffmpeg(
            "-i", str(path), "-af", f"asetpts=PTS*{speed}+{EPOCH + start}/TB",
            "-c:a", "pcm_s16le", "-f", "nut", str(target),
        )
        probe = json.loads(command(
            FFPROBE, "-v", "error", "-show_entries", "format=start_time",
            "-of", "json", str(target),
        ))
        self.assertAlmostEqual(float(probe["format"]["start_time"]), EPOCH + start, delta=2 / rate)
        return target

    def paired(self, reference: Path, microphone: Path, graph: str = FILTER) -> bytes:
        # Same timestamp normalization, isync reference, mapping and finite
        # output as Input::arguments + capture::arguments. Only the demuxers
        # differ: generated files instead of real Pulse sources.
        return ffmpeg(
            "-copyts", "-start_at_zero", "-i", str(reference),
            "-isync", "0", "-i", str(microphone),
            "-filter_complex", graph, "-map", "[paired]", "-t", str(WINDOW),
            "-vn", "-ac", "2", "-ar", str(RATE), "-c:a", "pcm_s16le",
            "-f", "s16le", "pipe:1",
        )

    def assert_lag(self, data: bytes, seconds: float, tolerance: int = 2) -> None:
        self.assertEqual(len(data), WINDOW * RATE * 4, "one exact finite stereo window")
        left, right = stereo(data)
        lag, confidence = best_lag(left, right)
        self.assertGreater(confidence, 0.88)
        self.assertLessEqual(abs(lag - round(seconds * RATE)), tolerance)

    def test_staggered_capture_starts_do_not_change_physical_lag(self) -> None:
        reference = self.media(0.0)
        for start in [0.0, 0.7, -0.5, 1.5, -1.0]:
            with self.subTest(microphone_start=start):
                microphone = self.media(start, 0.4)
                self.assert_lag(self.paired(reference, microphone), 0.4)

    def test_resampling_and_downmixing_keep_reference_left_and_mic_right(self) -> None:
        for reference_rate, microphone_rate, channels in [(48_000, 48_000, 2), (44_100, 48_000, 1)]:
            with self.subTest(reference_rate=reference_rate, microphone_rate=microphone_rate, channels=channels):
                reference = self.media(0.0, rate=reference_rate, channels=2)
                microphone = self.media(0.275, 0.61525, rate=microphone_rate, channels=channels)
                self.assert_lag(self.paired(reference, microphone), 0.61525)

    def test_timestamped_sample_clock_drift_is_resampled_onto_the_common_timeline(self) -> None:
        reference = self.media(0.0, rate=48_000, channels=2)
        for speed in [0.999, 1.001]:
            with self.subTest(clock_ratio=speed):
                microphone = self.media(0.7, 0.4, rate=48_000, channels=2, speed=speed)
                self.assert_lag(self.paired(reference, microphone), 0.4, tolerance=16)

    def test_independent_zeroing_is_a_detectable_negative_control(self) -> None:
        reference = self.media(0.0)
        microphone = self.media(0.7, 0.4)
        wrong_graph = FILTER.replace("[0:a]", "[0:a]asetpts=PTS-STARTPTS,").replace(
            "[1:a]", "[1:a]asetpts=PTS-STARTPTS,",
        )
        left, right = stereo(self.paired(reference, microphone, wrong_graph))
        lag, confidence = best_lag(left, right)
        self.assertGreater(confidence, 0.88)
        self.assertAlmostEqual(lag / RATE, -0.3, delta=2 / RATE)
        self.assert_lag(self.paired(reference, microphone), 0.4)

    def test_short_capture_is_not_padded_into_false_completion(self) -> None:
        reference = self.media(0.0)
        microphone = self.media(0.7, 0.4, duration=1.0)
        data = self.paired(reference, microphone)
        self.assertGreater(len(data), 0)
        self.assertLess(len(data), WINDOW * RATE * 4)

    def test_silence_does_not_invent_a_reference_signal(self) -> None:
        reference = self.media(0.0, silence=True)
        microphone = self.media(0.7, silence=True)
        data = self.paired(reference, microphone)
        self.assertEqual(len(data), WINDOW * RATE * 4)
        self.assertFalse(any(data))


if __name__ == "__main__":
    for executable in [FFMPEG, FFPROBE]:
        if not shutil.which(executable):
            raise SystemExit(f"required test executable not installed: {executable}")
    print(command(FFMPEG, "-version").decode().splitlines()[0], flush=True)
    unittest.main(verbosity=2)
