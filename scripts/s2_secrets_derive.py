#!/usr/bin/env python3
"""Derive the Sonos S2 app's obfuscated secrets from libsecrets.so.

The app's `com.sonos.secrets.Secrets` JNI layer stores each secret as 36 bytes
XORed with the SHA-256 hex of the package name it is asked about.  Every
`Java_com_sonos_secrets_Secrets_get*` function is the same shape:

    adr  x25, <blob>          ; 36-byte obfuscated secret
    ... sha256(packageName, sp+0x27) ...
    loop 0x24 times: out[i] = blob[i] ^ hex[i % strlen(hex)]

so the plaintext is recoverable offline from the .so alone.  The package name
the app passes is the literal "com.sonos.secrets".

Usage: s2_secrets_derive.py <libsecrets.so> [package-name]

Prints one line per `adr x25` site: the file offset, the derived secret and the
name of the JNI function it belongs to.  Non-printable results are the
non-string secrets (e.g. the Braze endpoint) and are shown escaped.
"""

import hashlib
import re
import subprocess
import sys


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    path = sys.argv[1]
    package = sys.argv[2] if len(sys.argv) > 2 else "com.sonos.secrets"

    data = open(path, "rb").read()
    dis = subprocess.run(
        ["llvm-objdump", "-d", path],
        check=True, capture_output=True, text=True,
    ).stdout

    # Map each `adr x25, <blob>` back to the JNI function that contains it.
    func = "?"
    key = hashlib.sha256(package.encode()).hexdigest().encode()
    for line in dis.splitlines():
        m = re.match(r"^[0-9a-f]+ <Java_com_sonos_secrets_Secrets_(\w+)>:", line)
        if m:
            func = m.group(1)
            continue
        m = re.match(r"^\s*[0-9a-f]+:\s+[0-9a-f ]+\tadr\tx25, 0x([0-9a-f]+)", line)
        if not m:
            continue
        off = int(m.group(1), 16)
        blob = data[off:off + 36]
        secret = bytes(b ^ key[i % len(key)] for i, b in enumerate(blob))
        printable = all(32 <= c < 127 for c in secret)
        shown = secret.decode() if printable else repr(secret)
        print(f"0x{off:04x} {func:28s} {shown}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
