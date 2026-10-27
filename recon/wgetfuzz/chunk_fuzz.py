#!/usr/bin/env python3
"""fuzz.py - run the corpus against the device's own busybox wget under qemu-user.

Each case: start chunk_srv.py on a free port, run

    qemu-ppc -L <root> ./bin/busybox wget -qC <log> -T 5 -O - http://127.0.0.1:<port>/x

(the exact argv the updater's execl uses, with -T shortened), then classify.

Classes, most severe first:
  SEGV   qemu reports "uncaught target signal 11" (guest fault)
  ABORT  guest died on another signal
  HANG   the 5 s timeout fired
  DIE    bb_error_msg_and_die path (message on stderr, rc 1)
  OK     clean exit

Results are one JSON line per case, appended as they happen, so a run can be
read while it is going. Deterministic: same corpus -> same order.

  ./fuzz.py --root ~/wgetfuzz/root --cases ~/wgetfuzz/cases --out run.jsonl
"""
import argparse
import json
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
TIMEOUT = 5


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def classify(rc, out, err):
    if b"uncaught target signal 11" in err or b"uncaught target signal 11" in out:
        return "SEGV"
    if rc == 124:
        return "HANG"
    if rc and rc >= 128:
        return "ABORT"
    if rc:
        return "DIE"
    return "OK"


def run_case(root, case, port, workdir, serve=1, stream=False):
    log = os.path.join(workdir, "wget.log")
    if os.path.exists(log):
        os.unlink(log)
    srv = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "chunk_srv.py"), "--port", str(port),
         "--file", case, "--serve", str(serve), "--repeat", "0" if stream else "1"],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    srv.stdout.readline()  # srv prints READY after listen()
    t0 = time.monotonic()
    try:
        p = subprocess.run(
            ["timeout", str(TIMEOUT), "qemu-ppc", "-L", root, "./bin/busybox",
             "wget", "-qC", log, "-T", "5", "-O", "-",
             "http://127.0.0.1:%d/x" % port],
            cwd=root, capture_output=True, timeout=TIMEOUT + 8,
        )
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        rc, out, err = 124, b"", b""
    dt = time.monotonic() - t0
    try:
        srv.wait(timeout=2)
    except subprocess.TimeoutExpired:
        srv.kill()
    logtxt = b""
    if os.path.exists(log):
        logtxt = open(log, "rb").read()[:4096]
    return rc, out, err, dt, logtxt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--cases", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--only", default="", help="substring filter on the case name")
    ap.add_argument("--serve", type=int, default=1, help="connections the server accepts")
    ap.add_argument("--stream", action="store_true", help="server repeats the body forever")
    a = ap.parse_args()

    cases = sorted(
        os.path.join(a.cases, f) for f in os.listdir(a.cases)
        if f.endswith(".http") and a.only in f
    )
    if not cases:
        print("no cases", file=sys.stderr)
        return 2

    counts = {}
    with open(a.out, "w") as sink:
        for case in cases:
            port = free_port()
            rc, out, err, dt, logtxt = run_case(a.root, case, port, os.path.dirname(a.out), a.serve, a.stream)
            cls = classify(rc, out, err)
            counts[cls] = counts.get(cls, 0) + 1
            rec = {
                "case": os.path.basename(case), "class": cls, "rc": rc,
                "secs": round(dt, 3), "out_len": len(out),
                "err": err.decode("latin-1", "replace")[:400],
                "log": logtxt.decode("latin-1", "replace")[:400],
            }
            sink.write(json.dumps(rec) + "\n")
            sink.flush()
            print("%-34s %-5s rc=%-3s %5.2fs out=%d %s"
                  % (rec["case"], cls, rc, dt, len(out),
                     rec["err"].strip().splitlines()[-1][:70] if rec["err"].strip() else ""),
                  flush=True)
    print("\n" + " ".join("%s=%d" % kv for kv in sorted(counts.items())))
    return 0


if __name__ == "__main__":
    sys.exit(main())
