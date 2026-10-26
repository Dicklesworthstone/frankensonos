#!/usr/bin/env python3
"""MDP3 / OTP blob tool (port of sonostool.py's blob_encdec, no pycryptodome).

  mdp3.py fields  MDP OTP                 list the three sealed fields
  mdp3.py decrypt MDP OTP rootfs|jffs|model
  mdp3.py upd     MDP OTP FILE OUTDIR     decrypt encrypted .upd sections
  mdp3.py luks    MDP OTP HEX32           derive the LUKS key
  mdp3.py selftest                        build a synthetic MDP3 and round-trip

MDP3 is 0x4000 bytes, magic f079a9cb; OTP is 0x100. The AES key is
sha256(OTP[0xD0:0xE0])[:16]; a field is {magic, u32le len, 8 bytes, data} and
its blob is ECB-keystream over IV||BE32(counter) XORed with the body, with the
last 12 bytes as IV XORed with the tag ("rootfs"/"ubifs"/"model").
"""
import hashlib
import hmac
import os
import struct
import subprocess
import sys

MAGIC_MDP3 = bytes.fromhex("f079a9cb")
MAGIC_FSKEY = bytes.fromhex("e84d8c61")
MAGIC_RSAKEY = bytes.fromhex("8dc6ccf4")
MAGIC_FW = bytes.fromhex("886499ca")
MAGIC_UPD = bytes.fromhex("497f1635")
MDP_LEN, OTP_LEN = 0x4000, 0x100
OFF = {"jffs": 0x0580, "rootfs": 0x0680, "model": 0x0780}
# fenway (model-8 family, trulyspinach/sonos-fenway includes/mdp.h): the same
# offsets but raw fixed-size fields instead of {magic, len, data} TLVs.
FENWAY_SIZE = {"jffs": 256, "rootfs": 256, "model": 2048}
# fenway key fields are {magic, len, reserved} + data, like the Amlogic TLVs,
# but with CAAM magics and a fixed capacity.
FENWAY_MAGIC = {
    "jffs": (bytes.fromhex("e7ada2f0"), bytes.fromhex("e84d8c61")),
    "rootfs": (bytes.fromhex("e84d8c61"), bytes.fromhex("e7ada2f0")),
    "model": (bytes.fromhex("46372819"), bytes.fromhex("8dc6ccf4")),
}
LAYOUT = "amlogic"
AUTO = False
SMDP = False
SMDP_MDP3_OFF = 512 + 4096  # struct smdp: MDP1 page, MDP2 page, then MDP3
TAG = {"jffs": b"ubifs", "rootfs": b"rootfs", "model": b"model"}


def aes_ecb(key, data):
    return subprocess.run(
        ["openssl", "enc", "-aes-128-ecb", "-nopad", "-K", key.hex()],
        input=data, capture_output=True, check=True).stdout


def xor(a, b):
    return bytes(x ^ y for x, y in zip(a, b))


def aes_key(otp):
    # only OTP[0xD0:0xE0] is used (the reference's single OTP read), so a
    # partial dump down to 0xE0 is enough.
    if len(otp) < 0xE0:
        sys.exit("wrong OTP length")
    return hashlib.sha256(otp[0xD0:0xE0]).digest()[:0x10]


def field(mdp, off, magic, kind=None):
    if len(mdp) != MDP_LEN or mdp[:4] != MAGIC_MDP3:
        sys.exit("not an MDP3 blob")
    if LAYOUT == "fenway":
        if mdp[off:off + 4] not in FENWAY_MAGIC[kind]:
            sys.exit("bad fenway field magic at 0x%x" % off)
        n = struct.unpack_from("<I", mdp, off + 4)[0]
        if n > FENWAY_SIZE[kind] - 12:
            sys.exit("fenway field at 0x%x overflows its slot" % off)
        return mdp[off + 12:off + 12 + n]
    if mdp[off:off + 4] != magic:
        sys.exit("bad field magic at 0x%x" % off)
    n = struct.unpack_from("<I", mdp, off + 4)[0]
    return mdp[off + 12:off + 12 + n]


def keymod(tag):
    return tag + b"\0" * (8 - len(tag)) if len(tag) < 8 else tag


def xor_mod(a, km):
    return bytes(x ^ km[i % len(km)] for i, x in enumerate(a))


def encdec(blob, otp, tag):
    key = aes_key(otp)
    ct_len = len(blob) - 12 - 16
    if ct_len % 0x10:
        sys.exit("body not AES-block aligned")
    ct = blob[:ct_len]
    iv = xor_mod(blob[ct_len:ct_len + 12], keymod(tag))
    ks = b"".join(iv + struct.pack(">I", 2 + i) for i in range(ct_len // 0x10))
    out = xor(aes_ecb(key, ks), ct)
    pad = out[-1]
    if not (0x20 <= len(out) and len(out) % 0x10 == 0 and pad <= 0x10
            and out[-pad:] == bytes([pad]) * pad):
        sys.exit("invalid padding")
    return out[:-pad]


def enc(plain, otp, tag):
    key = aes_key(otp)
    pad = 0x10 - len(plain) % 0x10
    plain = plain + bytes([pad]) * pad
    iv = os.urandom(12)
    ks = b"".join(iv + struct.pack(">I", 2 + i) for i in range(len(plain) // 0x10))
    ct = xor(aes_ecb(key, ks), plain)
    return ct + xor_mod(iv, keymod(tag)) + os.urandom(16)


def sections(d):
    o = 0
    while o < len(d):
        if d[o:o + 4] != MAGIC_UPD:
            sys.exit("bad .upd magic at 0x%x" % o)
        _t, size, _f = struct.unpack_from("<III", d, o + 4)
        yield o, size
        o += size


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    global LAYOUT, SMDP, AUTO
    argv = [a for a in sys.argv[1:] if not a.startswith("--layout=") and a != "--smdp"]
    for a in sys.argv[1:]:
        if a.startswith("--layout="):
            LAYOUT = a.split("=", 1)[1]
        if a == "--smdp":
            SMDP = True
        if a == "--layout=auto":
            AUTO = True
    sys.argv = [sys.argv[0]] + argv
    cmd = sys.argv[1]
    if cmd == "selftest":
        otp = bytes(range(0x100))
        mdp = bytearray(MAGIC_MDP3 + b"\0" * (MDP_LEN - 4))
        for kind, off, magic in (("rootfs", OFF["rootfs"], MAGIC_FSKEY),
                                 ("model", OFF["model"], MAGIC_RSAKEY)):
            blob = enc(b"secret-" + kind.encode() + b"-" * 32, otp, TAG[kind])
            mdp[off:off + 4] = magic
            struct.pack_into("<I", mdp, off + 4, len(blob))
            mdp[off + 12:off + 12 + len(blob)] = blob
        for kind, off, magic in (("rootfs", OFF["rootfs"], MAGIC_FSKEY),
                                 ("model", OFF["model"], MAGIC_RSAKEY)):
            got = encdec(field(bytes(mdp), off, magic, kind), otp, TAG[kind])
            assert got == b"secret-" + kind.encode() + b"-" * 32, (kind, got)
        print("selftest ok")
        return
    if cmd == "upd-pem":
        mdp = otp = None
    elif cmd == "otp":
        mdp = b""
        otp = open(sys.argv[2], "rb").read()
    else:
        mdp = open(sys.argv[2], "rb").read()
        otp = open(sys.argv[3], "rb").read()
        if SMDP:
            if len(mdp) < SMDP_MDP3_OFF + MDP_LEN:
                sys.exit("input too short for an smdp image")
            mdp = mdp[SMDP_MDP3_OFF:SMDP_MDP3_OFF + MDP_LEN]
        if AUTO:
            probe = mdp[OFF["jffs"]:OFF["jffs"] + 4]
            if probe in FENWAY_MAGIC["jffs"]:
                LAYOUT = "fenway"
            elif probe == MAGIC_FSKEY:
                LAYOUT = "amlogic"
            else:
                sys.exit("cannot detect layout at 0x%x" % OFF["jffs"])
    if cmd == "fields":
        for kind, off, magic in (("jffs", OFF["jffs"], MAGIC_FSKEY),
                                 ("rootfs", OFF["rootfs"], MAGIC_FSKEY),
                                 ("model", OFF["model"], MAGIC_RSAKEY)):
            try:
                blob = field(mdp, off, magic, kind)
            except SystemExit as e:
                print("%-6s absent (%s)" % (kind, e))
                continue
            try:
                plain = encdec(blob, otp, TAG[kind])
                print("%-6s %d bytes -> %d plaintext bytes" % (kind, len(blob), len(plain)))
            except SystemExit as e:
                print("%-6s %d bytes, decrypt failed (%s)" % (kind, len(blob), e))
    elif cmd == "decrypt":
        kind = sys.argv[4]
        sys.stdout.buffer.write(encdec(field(mdp, OFF[kind], MAGIC_FSKEY if kind != "model" else MAGIC_RSAKEY, kind), otp, TAG[kind]))
    elif cmd == "upd-pem":
        data = open(sys.argv[2], "rb").read()
        outdir = sys.argv[3]
        os.makedirs(outdir, exist_ok=True)
        for i, (off, size) in enumerate(sections(data), 1):
            body = data[off + 16:off + size]
            if body[:4] != MAGIC_FW:
                continue
            aes = subprocess.run(
                ["openssl", "pkeyutl", "-decrypt", "-inkey", sys.argv[4],
                 "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha1"],
                input=body[0x32:0x132], capture_output=True, check=True).stdout
            plain = subprocess.run(
                ["openssl", "enc", "-aes-128-cbc", "-d", "-K", aes.hex(), "-iv", "00" * 16],
                input=body[0x13A:], capture_output=True, check=True).stdout
            open(os.path.join(outdir, "%02d.bin" % i), "wb").write(plain[0x10:])
            print("section %d -> %02d.bin" % (i, i))
    elif cmd == "upd":
        data = open(sys.argv[4], "rb").read()
        outdir = sys.argv[5]
        os.makedirs(outdir, exist_ok=True)
        key = encdec(field(mdp, OFF["model"], MAGIC_RSAKEY, "model"), otp, TAG["model"])
        for i, (off, size) in enumerate(sections(data), 1):
            body = data[off + 16:off + size]
            if body[:4] != MAGIC_FW:
                continue
            wrapped = body[0x32:0x132]
            aes = subprocess.run(
                ["openssl", "pkeyutl", "-decrypt", "-inkey", "/dev/stdin",
                 "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha1"],
                input=wrapped, capture_output=True, check=True).stdout
            plain = subprocess.run(
                ["openssl", "enc", "-aes-128-cbc", "-d", "-K", aes.hex(), "-iv", "00" * 16],
                input=body[0x13A:], capture_output=True, check=True).stdout
            open(os.path.join(outdir, "%02d.bin" % i), "wb").write(plain[0x10:])
            print("section %d -> %02d.bin" % (i, i))
    elif cmd == "otp":
        k = aes_key(otp)
        print("aes key sha256(otp[0xD0:0xE0])[:16] =", k.hex())
        if otp[0xD0:0xE0] == b"\0" * 16:
            print("warning: OTP slice is all zero")
    elif cmd == "model-pem":
        pem = encdec(field(mdp, OFF["model"], MAGIC_RSAKEY, "model"), otp, TAG["model"])
        sys.stdout.buffer.write(pem)
    elif cmd == "verify":
        import tempfile
        pem = encdec(field(mdp, OFF["model"], MAGIC_RSAKEY, "model"), otp, TAG["model"])
        with tempfile.NamedTemporaryFile(suffix=".pem") as kf:
            kf.write(pem)
            kf.flush()
            data = open(sys.argv[4], "rb").read()
            for i, (off, size) in enumerate(sections(data), 1):
                body = data[off + 16:off + size]
                if body[:4] != MAGIC_FW:
                    continue
                aes = subprocess.run(
                    ["openssl", "pkeyutl", "-decrypt", "-inkey", kf.name,
                     "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha1"],
                    input=body[0x32:0x132], capture_output=True, check=True).stdout
                plain = subprocess.run(
                    ["openssl", "enc", "-aes-128-cbc", "-d", "-K", aes.hex(), "-iv", "00" * 16],
                    input=body[0x13A:], capture_output=True, check=True).stdout[0x10:]
                print("section %d %s" % (i, "squashfs" if plain[:4] == b"hsqs" else plain[:4].hex()))
    elif cmd == "luks":
        key_in = bytes.fromhex(sys.argv[4])
        if len(key_in) != 0x20 or key_in[:16] not in (b"\0" * 16, b"\xff" * 16):
            sys.exit("bad key")
        kind = "jffs" if key_in[0] == 0 else "rootfs"
        k = encdec(field(mdp, OFF[kind], MAGIC_FSKEY, kind), otp, TAG[kind])
        a = b"sonos luks" + key_in
        h = hmac.new(k, a, hashlib.sha256)
        print(hmac.new(k, h.digest() + a, hashlib.sha256).hexdigest(), kind)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
