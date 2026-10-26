#!/usr/bin/env python3
"""Known-vector differential: our mdp3.py vs the published sonostool.py.

Builds a synthetic MDP3 (jffs/rootfs/model fields) and OTP, decrypts every
field with both implementations and asserts byte equality, then compares the
LUKS derivation. Usage: vector_test.py PATH_TO_SONOSTOOL_PY [REF_PYTHON]
"""
import hashlib
import importlib.util
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import mdp3  # noqa: E402

ref_path = sys.argv[1]
ref_py = sys.argv[2] if len(sys.argv) > 2 else sys.executable

spec = importlib.util.spec_from_file_location("sonostool", ref_path)
ref = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ref)

otp = bytes((i * 7 + 3) & 0xFF for i in range(0x100))
mdp = bytearray(mdp3.MAGIC_MDP3 + b"\0" * (mdp3.MDP_LEN - 4))
plain = {}
for kind, off, magic in (("jffs", mdp3.OFF["jffs"], mdp3.MAGIC_FSKEY),
                         ("rootfs", mdp3.OFF["rootfs"], mdp3.MAGIC_FSKEY),
                         ("model", mdp3.OFF["model"], mdp3.MAGIC_RSAKEY)):
    body = (b"%s-key-" % kind.encode()) + bytes(range(0x30))
    plain[kind] = body
    blob = mdp3.enc(body, otp, mdp3.TAG[kind])
    mdp[off:off + 4] = magic
    import struct
    struct.pack_into("<I", mdp, off + 4, len(blob))
    mdp[off + 12:off + 12 + len(blob)] = blob

with tempfile.TemporaryDirectory() as td:
    mp, op = os.path.join(td, "mdp.bin"), os.path.join(td, "otp.bin")
    open(mp, "wb").write(mdp)
    open(op, "wb").write(otp)
    print("mdp sha256", hashlib.sha256(mdp).hexdigest())
    print("otp sha256", hashlib.sha256(otp).hexdigest())

    # reference: decrypt through the published class
    tool = ref.SONOSTool(bytes(mdp), otp)
    ref_out = {"jffs": tool.jffs_key, "rootfs": tool.rootfs_key,
               "model": tool.model_rsakey}

    # ours: same inputs through the CLI
    for kind in ("jffs", "rootfs", "model"):
        got = subprocess.run([sys.executable, os.path.join(HERE, "mdp3.py"),
                              "decrypt", mp, op, kind],
                             capture_output=True, check=True).stdout
        assert got == ref_out[kind], (kind, got[:16], ref_out[kind][:16])
        assert got == plain[kind], kind
        print("%-6s %d bytes, identical to reference and to the plaintext" % (kind, len(got)))

    # LUKS derivation, both sides
    key_in = b"\0" * 16 + bytes(range(0x10))
    ours = subprocess.run([sys.executable, os.path.join(HERE, "mdp3.py"),
                           "luks", mp, op, key_in.hex()],
                          capture_output=True, check=True).stdout.split()[0].decode()
    theirs = tool.sonos_luks_key(key_in).hex()
    assert ours == theirs, (ours, theirs)
    print("luks   identical:", ours)
print("VECTOR OK")
