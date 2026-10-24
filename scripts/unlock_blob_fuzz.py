#!/usr/bin/env python3
"""Pre-auth unlock-blob fuzzers for the S2 :1443 unauthenticated handlers.

Both routes take a urlencoded `confirm=` value, percent-decode it, strip
whitespace, hex-deserialize into 0x400 bytes and hand it to the RSA verifier.
The GET issues the per-request challenge/state and gates the POST.

  devunlock <ip>   /devunlock, 15 cases
  mfg       <ip>   /mfgunlock, 5 cases

Each case: GET, POST, then /info liveness. Records code/size/time; a 403 means
the blob reached the verifier, 400 is a parse/state reject.
"""
import http.client, json, ssl, sys, time, urllib.request

CTX = ssl._create_unverified_context()


def req(ip, path, body=None):
    c = http.client.HTTPSConnection(ip, 1443, context=CTX, timeout=20)
    t0 = time.time()
    try:
        h = {"Content-Type": "application/x-www-form-urlencoded"} if body is not None else {}
        c.request("POST" if body is not None else "GET", path, body=body, headers=h)
        r = c.getresponse(); d = r.read()
        return r.status, len(d), round(time.time() - t0, 3), None
    except Exception as e:
        return None, None, round(time.time() - t0, 3), type(e).__name__
    finally:
        try: c.close()
        except Exception: pass


def info(ip):
    try:
        urllib.request.urlopen("http://%s:1400/info" % ip, timeout=3).read(); return True
    except Exception:
        return False


def run(ip, path, cases):
    out = []
    for label, val in cases:
        g = req(ip, path)
        code, size, dt, err = req(ip, path, (val or "").encode())
        up = info(ip)
        rec = {"case": label, "body": len(val or ""), "get": g[0], "code": code,
               "size": size, "time": dt, "err": err, "info": up}
        print(json.dumps(rec), flush=True); out.append(rec)
        time.sleep(1.2)
    return out


def dev(ip):
    hex4 = "deadbeef"
    big = lambda n: ("ab" * n)[:2 * n]          # n decoded bytes
    magic = "28721e71" + "00000014" + "00" * 16 + "00000055" + "00000057"
    cases = [
        ("none", None),
        ("empty", "confirm="),
        ("odd-1", "confirm=a"),
        ("two-bytes", "confirm=" + hex4[:4]),
        ("0x400", "confirm=" + big(0x400)),
        ("0x800", "confirm=" + big(0x800)),
        ("0x801", "confirm=" + big(0x801)),
        ("64KB", "confirm=" + big(0x10000)),
        ("non-hex", "confirm=zzzz"),
        ("percent", "confirm=%64%65%61%64%62%65%65%66"),
        ("nul", "confirm=" + hex4 + "%00" + hex4),
        ("mid", "x=1&confirm=" + hex4),
        ("end", "confirm=" + hex4 + "&x=1"),
        ("dup", "confirm=" + hex4 + "&confirm=11223344"),
        ("magic-badring", "confirm=" + magic + "00000000"),
        ("plus-term", "confirm=" + hex4 + "+junk"),
    ]
    return run(ip, "/devunlock", cases)


def mfg(ip):
    big = lambda n: ("cd" * n)[:2 * n]
    cases = [
        ("empty", "confirm="),
        ("4KB", "confirm=" + big(4096)),
        ("64KB", "confirm=" + big(65536)),
        ("dup", "confirm=cafe&confirm=beef"),
    ]
    return run(ip, "/mfgunlock", cases)


if __name__ == "__main__":
    which, ip = sys.argv[1], sys.argv[2]
    dev(ip) if which == "devunlock" else mfg(ip)
