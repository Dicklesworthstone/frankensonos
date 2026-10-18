#!/usr/bin/env python3
"""Test the vendored resource publisher without changing the source workspace."""

from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib


def package_pins(lock):
    return {
        (p["name"], p["version"], p["source"], p.get("checksum"))
        for p in tomllib.loads(lock.decode())["package"]
        if "source" in p
    }


def main():
    root = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
    original_manifest = (root / "Cargo.toml").read_bytes()
    original_lock = (root / "Cargo.lock").read_bytes()
    work = Path(tempfile.mkdtemp(prefix="fsonos-vendor-tests-")) / "repo"
    print(f"Retaining vendor test workspace at {work}", flush=True)
    shutil.copytree(root, work, ignore=shutil.ignore_patterns(".git", "target", "node_modules"))
    manifest = original_manifest.decode()
    excluded = '    "vendor/fastmcp-server",\n'
    if manifest.count(excluded) != 1 or manifest.count("members = [\n") != 1:
        raise RuntimeError("expected the vendored server to be excluded from the root workspace")
    manifest = manifest.replace(excluded, "", 1).replace(
        "members = [\n", "members = [\n" + excluded, 1
    )
    (work / "Cargo.toml").write_text(manifest)
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(Path(env.get("CARGO_TARGET_DIR", root / "target")).resolve())
    # Only the copy gains the server's missing dev dependencies. Cargo retains
    # existing non-workspace packages, and the explicit pin check enforces that.
    subprocess.run(["cargo", "update", "--workspace"], cwd=work, env=env, check=True)
    resolved = (work / "Cargo.lock").read_bytes()
    missing = package_pins(original_lock) - package_pins(resolved)
    if missing:
        raise RuntimeError(f"vendor tests changed existing dependency pins: {sorted(missing)}")
    if (root / "Cargo.toml").read_bytes() != original_manifest or (root / "Cargo.lock").read_bytes() != original_lock:
        raise RuntimeError("the original workspace manifest or lockfile changed")
    for test_args in (
        ["--lib", "resource_updates::tests"],
        ["--lib", "http_identity::tests"],
        ["--test", "http_native_identity"],
        ["--no-default-features", "--test", "http_native_identity"],
    ):
        subprocess.run(
            ["cargo", "test", "--locked", "-p", "fastmcp-server", *test_args],
            cwd=work, env=env, check=True,
        )


if __name__ == "__main__":
    main()
