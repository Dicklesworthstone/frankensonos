#!/usr/bin/env python3
"""Exercise the shipped calibration signal with real FFmpeg, without audio devices.

Python standard library only. These are signal/codec/fixture-process checks,
not execution of the Rust CLI, Pulse routing, the Rust estimator or Sonos.
"""
from __future__ import annotations

import array
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SIGNAL = (ROOT / "crates/fsonos-cli/src/streaming/synchronization/probe.lavfi").read_text().strip()
FFMPEG = os.environ.get("FFMPEG", "ffmpeg")
RATE = 48_000


def run(*args: str, data: bytes | None = None) -> bytes:
    kwargs = {"stdin": subprocess.DEVNULL} if data is None else {"input": data}
    result = subprocess.run(
        [FFMPEG, "-hide_banner", "-loglevel", "error", "-nostdin", *args],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, check=False, **kwargs,
    )
    if result.returncode:
        raise AssertionError(result.stderr.decode(errors="replace"))
    return result.stdout


def render(seconds: int) -> bytes:
    return run(
        "-f", "lavfi", "-i", f"{SIGNAL},afade=t=out:st={seconds - 1}:d=1",
        "-t", str(seconds), "-vn", "-ac", "2", "-ar", str(RATE),
        "-c:a", "pcm_s16le", "-f", "s16le", "pipe:1",
    )


def samples(data: bytes) -> list[int]:
    result = array.array("h")
    result.frombytes(data)
    if sys.byteorder != "little":
        result.byteswap()
    return list(result)


def rms(values: list[int]) -> float:
    return math.sqrt(sum(value * value for value in values) / len(values)) / 32_768


def at_eight_k(data: bytes, encoded: bool = False) -> list[int]:
    input_args = [] if encoded else ["-f", "s16le", "-ar", str(RATE), "-ac", "2"]
    return samples(run(
        *input_args, "-i", "pipe:0", "-ac", "1", "-ar", "8000",
        "-c:a", "pcm_s16le", "-f", "s16le", "pipe:1", data=data,
    ))


def best_lag(left: list[int], right: list[int]) -> tuple[float, float]:
    # Independent sparse correlation oracle, not the production GCC-PHAT.
    anchors = list(range(16_000, 24_000, 41))
    x = [left[n] for n in anchors]
    average = sum(x) / len(x)
    x = [value - average for value in x]
    power = sum(value * value for value in x)

    def score(lag: float) -> float:
        whole = math.floor(lag)
        fraction = lag - whole
        y = [right[n + whole] * (1 - fraction) + right[n + whole + 1] * fraction
             for n in anchors]
        average_y = sum(y) / len(y)
        y = [value - average_y for value in y]
        denominator = math.sqrt(power * sum(value * value for value in y))
        return sum(a * b for a, b in zip(x, y)) / denominator if denominator else 0.0

    coarse = max(range(0, 8_001, 4), key=score)
    integer = max(range(max(0, coarse - 4), coarse + 5), key=score)
    # MP3 delay at 44100 Hz is not an integral number of 8000 Hz samples.
    # Preserve the strong quality assertion using a sub-sample oracle rather
    # than lowering it to accommodate a half-sample alignment mismatch.
    lag = max((max(0.0, integer + step / 20) for step in range(-10, 11)), key=score)
    return lag, score(lag)


class ProbeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.raw = render(12)
        cls.mono = samples(cls.raw)[::2]

    def test_exact_finite_windows_and_digital_amplitude_ceiling(self) -> None:
        for seconds in [12, 20, 30]:
            with self.subTest(seconds=seconds):
                data = self.raw if seconds == 12 else render(seconds)
                self.assertEqual(len(data), seconds * RATE * 4)
                values = samples(data)
                self.assertLessEqual(max(map(abs, values)), math.ceil(0.025 * 32_768))
                self.assertGreater(rms(values[RATE * 2:RATE * 6]), 0.001)

    def test_reference_is_identical_on_both_channels(self) -> None:
        values = samples(self.raw)
        self.assertEqual(values[::2], values[1::2])

    def test_fades_do_not_start_or_finish_at_full_level(self) -> None:
        steady = rms(self.mono[RATE:2 * RATE])
        self.assertLess(rms(self.mono[:480]), steady * 0.15)
        self.assertLess(rms(self.mono[-480:]), steady * 0.03)
        self.assertEqual(self.mono[0], 0)
        self.assertLessEqual(abs(self.mono[-1]), 1)

    def test_independent_windows_do_not_repeat_the_same_noise(self) -> None:
        other = render(12)
        self.assertNotEqual(self.raw[RATE * 4:RATE * 8], other[RATE * 4:RATE * 8])

    def test_signal_contains_no_material_dc_offset(self) -> None:
        values = self.mono[RATE:10 * RATE]
        mean = sum(values) / len(values) / 32_768
        self.assertLess(abs(mean), 0.00001)

    def test_matching_audio_survives_the_stream_mp3_settings(self) -> None:
        encoded = run(
            "-f", "s16le", "-ar", str(RATE), "-ac", "2", "-i", "pipe:0",
            "-vn", "-ar", "44100", "-c:a", "libmp3lame", "-reservoir", "0",
            "-b:a", "192k", "-f", "mp3", "-write_xing", "0",
            "-id3v2_version", "0", "-flush_packets", "1", "pipe:1", data=self.raw,
        )
        reference = at_eight_k(self.raw)
        decoded = at_eight_k(encoded, encoded=True)
        codec_lag, confidence = best_lag(reference, decoded)
        self.assertGreater(confidence, 0.8)
        self.assertLess(codec_lag, 400)
        # Report the codec offset separately; do not mistake it for acoustic lag.
        delayed = [0] * 3_200 + decoded
        observed, confidence = best_lag(reference, delayed)
        self.assertGreater(confidence, 0.8)
        self.assertLessEqual(abs(observed - codec_lag - 3_200), 1)
        print(f"fixture codec offset={codec_lag}/8000 s; injected delay=3200/8000 s", flush=True)

    def test_real_time_fixture_can_be_stopped_and_reaped(self) -> None:
        child = subprocess.Popen(
            [FFMPEG, "-hide_banner", "-loglevel", "error", "-nostdin", "-re",
             "-f", "lavfi", "-i", f"{SIGNAL},afade=t=out:st=11:d=1",
             "-t", "12", "-f", "null", "-"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        try:
            time.sleep(0.2)
            self.assertIsNone(child.poll(), "fixture remains live until explicitly stopped")
            started = time.monotonic()
            child.kill()
            child.communicate(timeout=3)
            self.assertLess(time.monotonic() - started, 3)
            self.assertIsNotNone(child.returncode)
        finally:
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=3)


if __name__ == "__main__":
    if shutil.which(FFMPEG) is None:
        raise SystemExit("FFmpeg is required; this is not a passing skip")
    print(run("-version").decode().splitlines()[0])
    unittest.main(verbosity=2)
