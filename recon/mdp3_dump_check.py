#!/usr/bin/env python3
"""Validate a candidate MDP3+OTP dump end to end, in one command.

  mdp3_dump_check.py DUMP OTP [--package UPD] [--expect FINGERPRINT]

DUMP may be the 0x4000 page or a whole smdp region (the page is then taken
from 0x1200). The layout is detected from the field magic, so an operator does
not have to know the hardware family. The model field is decrypted, the key's
recipient fingerprint is computed (sha1 of the DER SubjectPublicKeyInfo, the
same id the Sonos envelopes carry) and compared with --expect (default: the
known model-16 recipient). With --package the real .upd is then decrypted and
each section's magic is reported.

Exit 0 only if every step holds. Exit 2 means the dump is not usable at all.
"""
import argparse
import hashlib
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "captures", "research", "mdp3"))
import mdp3  # noqa: E402

MODEL16 = "fd88f2642a9c89a44747c7b4342cdb483fe1d437"


def detect(mdp):
    """Return (layout, page) or exit 2."""
    if len(mdp) >= mdp3.SMDP_MDP3_OFF + mdp3.MDP_LEN:
        page = mdp[mdp3.SMDP_MDP3_OFF:mdp3.SMDP_MDP3_OFF + mdp3.MDP_LEN]
        if page[:4] == mdp3.MAGIC_MDP3:
            return "fenway-or-amlogic", page, "smdp 0x1200"
    if mdp[:4] == mdp3.MAGIC_MDP3:
        return "page", mdp, "direct"
    sys.exit("no MDP3 magic at 0 or 0x1200 (not a usable dump)")


def layout_of(page):
    # the CAAM magics are unique to fenway; the shared FSKEY magic is not.
    jffs = page[mdp3.OFF["jffs"]:mdp3.OFF["jffs"] + 4]
    model = page[mdp3.OFF["model"]:mdp3.OFF["model"] + 4]
    if jffs == mdp3.FENWAY_MAGIC["jffs"][0] or model == mdp3.FENWAY_MAGIC["model"][0]:
        return "fenway"
    if jffs == mdp3.MAGIC_FSKEY or model == mdp3.MAGIC_RSAKEY:
        return "amlogic"
    sys.exit("cannot detect the layout at 0x580/0x780")


def fingerprint(pem):
    der = subprocess.run(["openssl", "pkey", "-pubout", "-outform", "DER"],
                         input=pem, capture_output=True, check=True).stdout
    return hashlib.sha1(der).hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dump")
    ap.add_argument("otp")
    ap.add_argument("--package")
    ap.add_argument("--expect", default=MODEL16)
    a = ap.parse_args()

    mdp = open(a.dump, "rb").read()
    otp = open(a.otp, "rb").read()
    if len(otp) < 0xE0:
        sys.exit("OTP shorter than 0xE0 (only [0xD0:0xE0] is used)")

    _, page, how = detect(mdp)
    mdp3.LAYOUT = layout_of(page)
    print("dump      %s (%s), layout %s" % (a.dump, how, mdp3.LAYOUT))
    print("otp       %s, %d bytes" % (a.otp, len(otp)))
    print("aes key   %s" % mdp3.aes_key(otp).hex())

    ok = True
    for kind in ("jffs", "rootfs"):
        try:
            plain = mdp3.encdec(mdp3.field(page, mdp3.OFF[kind],
                                           mdp3.MAGIC_FSKEY, kind),
                                otp, mdp3.TAG[kind])
            print("%-9s decrypt ok, %d bytes" % (kind, len(plain)))
        except SystemExit as e:
            print("%-9s FAIL (%s)" % (kind, e))
            ok = False

    try:
        pem = mdp3.encdec(mdp3.field(page, mdp3.OFF["model"],
                                     mdp3.MAGIC_RSAKEY, "model"),
                          otp, mdp3.TAG["model"])
    except SystemExit as e:
        sys.exit("model field FAIL (%s) — no key to test" % e)
    fp = fingerprint(pem)
    print("model key %d bytes, recipient %s" % (len(pem), fp))
    if fp != a.expect:
        print("MISMATCH  expected %s" % a.expect)
        ok = False
    else:
        print("recipient matches %s" % a.expect)

    if a.package:
        with tempfile.NamedTemporaryFile(suffix=".pem") as kf:
            kf.write(pem)
            kf.flush()
            data = open(a.package, "rb").read()
            for i, (off, size) in enumerate(mdp3.sections(data), 1):
                body = data[off + 16:off + size]
                if body[:4] != mdp3.MAGIC_FW:
                    continue
                try:
                    aes = subprocess.run(
                        ["openssl", "pkeyutl", "-decrypt", "-inkey", kf.name,
                         "-pkeyopt", "rsa_padding_mode:oaep",
                         "-pkeyopt", "rsa_oaep_md:sha1"],
                        input=body[0x32:0x132], capture_output=True,
                        check=True).stdout
                    plain = subprocess.run(
                        ["openssl", "enc", "-aes-128-cbc", "-d", "-K", aes.hex(),
                         "-iv", "00" * 16],
                        input=body[0x13A:], capture_output=True, check=True).stdout
                except subprocess.CalledProcessError:
                    print("section %-2d FAIL" % i)
                    ok = False
                    continue
                print("section %-2d %s" % (i, plain[0x10:0x14].hex()))

    print("RESULT    %s" % ("OK" if ok else "FAIL"))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
