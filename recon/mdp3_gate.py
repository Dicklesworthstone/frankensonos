#!/usr/bin/env python3
"""Regression gate for the MDP3 tool: every proof the lane has, in one run.

  mdp3_gate.py [--reference DIR]

Steps: selftest, the differential vector against blasty's sonostool (skipped
when the reference env is absent), an amlogic and a fenway round-trip, the
legacy model-8 envelope, and a decrypt of the real 57.23 package. Exit 0 only
if every step passes, so a later edit cannot quietly break a proof.
"""
import argparse
import os
import struct
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TOOL = os.path.join(ROOT, "captures", "research", "mdp3", "mdp3.py")
sys.path.insert(0, os.path.join(ROOT, "captures", "research", "mdp3"))
import mdp3  # noqa: E402

PKG = os.path.join(ROOT, "captures", "firmware", "57.23-74170-1-16",
                   "57.23-74170-1-16.upd")
PEM = os.path.join(ROOT, "captures", "firmware", "57.23-74170-1-16",
                   "model16-private.pem")
OTP = bytes((i * 7 + 3) & 0xFF for i in range(0x100))
fails = []


def step(name, fn):
    try:
        detail = fn()
        print("PASS %-22s %s" % (name, detail or ""))
    except Exception as e:  # noqa: BLE001 - a gate reports, never raises
        print("FAIL %-22s %s" % (name, e))
        fails.append(name)


def run(*args, **kw):
    return subprocess.run([sys.executable, TOOL] + list(args),
                          capture_output=True, text=True, **kw)


def page(layout):
    mdp = bytearray(mdp3.MAGIC_MDP3 + b"\0" * (mdp3.MDP_LEN - 4))
    struct.pack_into("<I", mdp, 4, 2)
    for kind, off, magic in (("jffs", 0x580, mdp3.MAGIC_FSKEY),
                             ("rootfs", 0x680, mdp3.MAGIC_FSKEY),
                             ("model", 0x780, mdp3.MAGIC_RSAKEY)):
        body = (layout + "-" + kind).encode() + b"-" * 32
        blob = mdp3.enc(body, OTP, mdp3.TAG[kind])
        if layout == "fenway":
            magic = mdp3.FENWAY_MAGIC[kind][0]
        mdp[off:off + 4] = magic
        struct.pack_into("<I", mdp, off + 4, len(blob))
        mdp[off + 12:off + 12 + len(blob)] = blob
    return bytes(mdp)


def real_page(tmp, name="real.mdp"):
    """A page whose model field is the real model16 key."""
    mdp = bytearray(page("amlogic"))
    blob = mdp3.enc(open(PEM, "rb").read(), OTP, mdp3.TAG["model"])
    mdp[0x780:0x784] = mdp3.MAGIC_RSAKEY
    struct.pack_into("<I", mdp, 0x784, len(blob))
    mdp[0x78C:0x78C + len(blob)] = blob
    return write(tmp, name, bytes(mdp))


def write(tmp, name, data):
    p = os.path.join(tmp, name)
    open(p, "wb").write(data)
    return p


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reference", default="/tmp/refvenv")
    a = ap.parse_args()

    with tempfile.TemporaryDirectory() as tmp:
        otp = write(tmp, "otp", OTP)

        step("selftest", lambda: run("selftest").stdout.strip())

        def vector():
            py = os.path.join(a.reference, "bin", "python")
            ref = "/tmp/blasty/sonostool/sonostool.py"
            if not (os.path.exists(py) and os.path.exists(ref)):
                raise RuntimeError("reference env absent")
            r = subprocess.run([py, os.path.join(ROOT, "captures", "research",
                                                 "mdp3", "vector_test.py"),
                                ref, py], capture_output=True, text=True)
            if "VECTOR OK" not in r.stdout:
                raise RuntimeError(r.stdout.strip()[-200:] or r.stderr[-200:])
            return "VECTOR OK"
        step("differential vector", vector)

        for layout in ("amlogic", "fenway"):
            def rt(layout=layout):
                p = write(tmp, layout + ".mdp", page(layout))
                r = run("--layout=" + layout, "decrypt", p, otp, "model")
                if r.returncode or not r.stdout.startswith(layout + "-model"):
                    raise RuntimeError(r.stdout.strip() or r.stderr.strip())
                return "round-trip"
            step(layout + " round-trip", rt)

        def legacy():
            pem = open(PEM, "rb").read()
            pub = subprocess.run(["openssl", "pkey", "-pubout"], input=pem,
                                 capture_output=True, check=True).stdout
            aes = bytes(range(16))
            with tempfile.NamedTemporaryFile(suffix=".pem") as pf:
                pf.write(pub)
                pf.flush()
                rsa = subprocess.run(
                    ["openssl", "pkeyutl", "-encrypt", "-pubin", "-inkey", pf.name,
                     "-pkeyopt", "rsa_padding_mode:oaep",
                     "-pkeyopt", "rsa_oaep_md:sha1"],
                    input=aes, capture_output=True, check=True).stdout
            body = b"legacy-model8-payload"
            pad = 16 - len(body) % 16
            plain = b"\x11" * 16 + body + bytes([pad]) * pad
            enc = subprocess.run(
                ["openssl", "enc", "-aes-128-cbc", "-e", "-K", aes.hex(),
                 "-iv", "00" * 16], input=plain, capture_output=True,
                check=True).stdout
            rec = bytes.fromhex(mdp3.MODEL16 if hasattr(mdp3, "MODEL16")
                                else "fd88f2642a9c89a44747c7b4342cdb483fe1d437")
            hlen = 0x16 + len(rec) + 4 + 4 + len(rsa) + 8
            env = (mdp3.MAGIC_FW + struct.pack(">I", hlen) + b"\0" * 12
                   + bytes([len(rec)]) + b"\0" + rec + b"\0" * 4
                   + struct.pack(">I", len(rsa)) + rsa + struct.pack(">I", 1)
                   + struct.pack(">I", len(enc)) + enc)
            sec = mdp3.MAGIC_UPD + struct.pack("<III", 3, 16 + len(env), 4) + env
            p = write(tmp, "legacy.upd", sec)
            r = run("--legacy-model8", "verify", real_page(tmp), otp, p)
            if "21 bytes" not in r.stdout:
                raise RuntimeError(r.stdout.strip() or r.stderr.strip())
            return "exact 21-byte payload"
        step("legacy model-8", legacy)

        def real():
            if not os.path.exists(PKG):
                raise RuntimeError("package absent")
            r = run("verify", real_page(tmp), otp, PKG)
            for want in ("23212f62", "48000005", "73717368"):
                if want not in r.stdout:
                    raise RuntimeError("missing %s in %r" % (want, r.stdout))
            return "sections 7/8/9"
        step("real package", real)

    print("\n%s (%d failed)" % ("GATE PASS" if not fails else "GATE FAIL",
                                len(fails)))
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
