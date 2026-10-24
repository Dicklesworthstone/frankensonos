#!/usr/bin/env python3
"""Run the device's own wget under qemu-ppc against the mutating server.

Guest setup (see docs/UPDATE_HTTP_CLIENT.md): a chroot of the extracted 1-16
rootfs at /tmp/sonos-ppc/root with qemu-ppc-static copied in as /qemu-ppc, and
the static server at /tmp/upd_server.py. Exact upstream argv is baked in.

Usage: python3 wget_offline_matrix.py MODE [MODE...]
"""
import json, os, subprocess, sys, time
MODES = sys.argv[1:]
ROOT = "/tmp/sonos-ppc/root"
WGET = "/tmp/qemu-ppc"
argv = [WGET, "-cpu", "e300c3", "/usr/bin/wget", "-qC", "/var/run/upgrade_report.txt",
        "-T", "5", "-O", "-", "http://127.0.0.1:8901/update.upd"]
for m in MODES:
    env = dict(os.environ, UPD_IMAGE="/tmp/upd_img.bin")
    srv = subprocess.Popen([sys.executable, "/tmp/upd_server.py", "serve", "--mode", m,
                            "--port", "8901", "--log", "/tmp/g.jsonl"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT, env=env)
    time.sleep(0.3)
    t0 = time.time()
    p = subprocess.run(["sudo", "chroot", ROOT] + argv, capture_output=True, timeout=15)
    dt = time.time() - t0
    srv.terminate(); srv.wait()
    print(json.dumps({"mode": m, "rc": p.returncode, "bytes": len(p.stdout),
                      "err": p.stderr.decode("latin1", "replace")[-90:], "sec": round(dt, 2)}), flush=True)
    if os.path.exists("/tmp/g.jsonl"):
        os.unlink("/tmp/g.jsonl")
