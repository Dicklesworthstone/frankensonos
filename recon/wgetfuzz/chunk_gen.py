#!/usr/bin/env python3
"""gen.py - deterministic chunked-grammar corpus for busybox wget 1.24.2.

Writes one file per case into the output dir: the exact bytes the server sends.
Every case is a full HTTP response, so a case is reproducible by hand:

    python3 srv.py --port 8900 --file cases/012-huge-chunk.http

The dimensions come from networking/wget.c in busybox 1.24.2 (retrieve_file_data,
gethdr, fgets_and_trim): the chunk-size line is parsed with STRTOOFF and never
validated, the chunk terminator is a blind fgets, the header value is truncated
at the copy buffer and the remainder is drained, and a truncated status line
leaves its tail to be parsed as a header.

Fixed seed, fixed order: the corpus is byte-identical on every run.
"""
import argparse
import os
import sys

BODY = b"0123456789abcdef" * 4  # 64 bytes of filler


def resp(headers, body=b"", status=b"HTTP/1.1 200 OK"):
    h = b"".join(x + b"\r\n" for x in headers)
    return status + b"\r\n" + h + b"\r\n" + body


def chunk(n, data=None, size_line=None):
    data = BODY[:n] if data is None else data
    line = size_line if size_line is not None else b"%x" % n
    return line + b"\r\n" + data + b"\r\n"


def cases():
    """(tag, bytes) pairs, in a fixed order."""
    TE = b"Transfer-Encoding: chunked"

    # --- control -------------------------------------------------------
    yield "control-one-chunk", resp([TE], chunk(16) + b"0\r\n\r\n")
    yield "control-three-chunks", resp([TE], chunk(8) + chunk(8) + chunk(8) + b"0\r\n\r\n")
    yield "control-clen", resp([b"Content-Length: 32"], BODY[:32])

    # --- chunk size line grammar --------------------------------------
    for tag, line in [
        ("size-0x-prefix", b"0x10"),
        ("size-upper-hex", b"10"),
        ("size-leading-space", b" 10"),
        ("size-trailing-space", b"10 "),
        ("size-plus", b"+10"),
        ("size-minus", b"-10"),
        ("size-empty", b""),
        ("size-junk", b"zz"),
        ("size-hex-then-junk", b"10zz"),
        ("size-16f", b"ffffffffffffffff"),
        ("size-15f", b"fffffffffffffff"),
        ("size-80000000", b"80000000"),
        ("size-7fffffff", b"7fffffff"),
        ("size-100000000", b"100000000"),
        ("size-ffffffff", b"ffffffff"),
        ("size-00000010", b"00000010"),
        ("size-tab", b"\t10"),
        ("size-cr-only", b"10\r"),
        ("size-ext", b"10;foo=bar"),
        ("size-ext-empty", b"10;"),
        ("size-quoted-ext", b'10;a="b c"'),
        ("size-1024-hex-digits", b"1" * 1024),
        ("size-4090-hex-digits", b"1" * 4090),
        ("size-4095-hex-digits", b"1" * 4095),
        ("size-4096-hex-digits", b"1" * 4096),
        ("size-8192-hex-digits", b"1" * 8192),
    ]:
        yield tag, resp([TE], line + b"\r\n" + BODY[:16] + b"\r\n0\r\n\r\n")

    # --- chunk terminators --------------------------------------------
    yield "term-lf-only", resp([TE], b"10\n" + BODY[:16] + b"\n0\n\n")
    yield "term-missing-after-data", resp([TE], b"10\r\n" + BODY[:16] + b"0\r\n\r\n")
    yield "term-double-crlf", resp([TE], b"10\r\n" + BODY[:16] + b"\r\n\r\n0\r\n\r\n")
    yield "term-cr-only", resp([TE], b"10\r" + BODY[:16] + b"\r0\r\r")
    yield "term-no-final-crlf", resp([TE], b"10\r\n" + BODY[:16] + b"\r\n0\r\n")

    # --- data length vs declared size ---------------------------------
    yield "data-short", resp([TE], b"100\r\n" + BODY[:16] + b"\r\n0\r\n\r\n")
    yield "data-long", resp([TE], b"4\r\n" + BODY[:16] + b"\r\n0\r\n\r\n")
    yield "data-exact-no-trailer", resp([TE], b"10\r\n" + BODY[:16])

    # --- transfer-encoding value grammar ------------------------------
    for tag, val in [
        ("te-upper", b"Transfer-Encoding: CHUNKED"),
        ("te-mixed", b"Transfer-Encoding: Chunked"),
        ("te-space", b"Transfer-Encoding:  chunked"),
        ("te-trailing-space", b"Transfer-Encoding: chunked "),
        ("te-tab", b"Transfer-Encoding:\tchunked"),
        ("te-empty", b"Transfer-Encoding:"),
        ("te-gzip", b"Transfer-Encoding: gzip"),
        ("te-list", b"Transfer-Encoding: chunked, gzip"),
        ("te-truncated", b"Transfer-Encoding: chunke"),
        ("te-dup", TE + b"\r\n" + TE),
        ("te-and-clen", TE + b"\r\nContent-Length: 8"),
        ("clen-and-te", b"Content-Length: 8\r\n" + TE),
        ("te-lower-name", b"transfer-encoding: chunked"),
        ("te-underscore-name", b"transfer_encoding: chunked"),
        ("te-dot-name", b"transfer.encoding: chunked"),
        ("te-space-before-colon", b"Transfer-Encoding : chunked"),
        ("te-no-colon", b"Transfer-Encoding chunked"),
        ("te-4096-value", b"Transfer-Encoding: " + b"c" * 4096),
        ("te-8192-value", b"Transfer-Encoding: " + b"c" * 8192),
    ]:
        yield tag, resp([val], chunk(16) + b"0\r\n\r\n")

    # --- content-length grammar ---------------------------------------
    for tag, val in [
        ("clen-huge", b"Content-Length: 99999999999999999999"),
        ("clen-negative", b"Content-Length: -1"),
        ("clen-hex", b"Content-Length: 0x20"),
        ("clen-empty", b"Content-Length:"),
        ("clen-junk", b"Content-Length: abc"),
        ("clen-4096-digits", b"Content-Length: " + b"9" * 4096),
    ]:
        yield tag, resp([val], BODY[:32])

    # --- status line grammar ------------------------------------------
    yield "status-100-then-200", resp([], b"", b"HTTP/1.1 100 Continue") + resp([], BODY[:8])
    yield "status-0-then-200", resp([], b"", b"HTTP/1.1 0 x") + resp([], BODY[:8])
    yield "status-4096-line", resp([], BODY[:8], b"HTTP/1.1 200 " + b"x" * 4096)
    yield "status-8192-line", resp([], BODY[:8], b"HTTP/1.1 200 " + b"x" * 8192)
    yield "status-no-space", resp([], BODY[:8], b"HTTP/1.1 200OK")
    yield "status-short", resp([], BODY[:8], b"HTTP/1.1 20")
    yield "status-empty", resp([], BODY[:8], b"")

    # --- header block grammar -----------------------------------------
    yield "hdr-4096-name", resp([b"x" * 4096 + b": v"], BODY[:8])
    yield "hdr-8192-name", resp([b"x" * 8192 + b": v"], BODY[:8])
    yield "hdr-empty-name", resp([b": v"], BODY[:8])
    yield "hdr-1000-headers", resp([b"x-h: v"] * 1000, BODY[:8])
    yield "hdr-1000-empty-lines", resp([b""] * 1000, BODY[:8])

    # --- trailer block after the terminal chunk ------------------------
    yield "trailer-headers", resp([TE], chunk(8) + b"0\r\nX-T: v\r\n\r\n")
    yield "trailer-huge", resp([TE], chunk(8) + b"0\r\n" + b"x" * 8192 + b"\r\n\r\n")
    yield "trailer-no-crlf", resp([TE], chunk(8) + b"0\r\n")

    # --- chunk counts --------------------------------------------------
    yield "chunks-1000", resp([TE], b"".join(chunk(1) for _ in range(1000)) + b"0\r\n\r\n")
    yield "chunks-1000-zero-len", resp([TE], b"0\r\n" * 1000)
    yield "chunks-1000-empty-lines", resp([TE], b"\r\n" * 1000 + b"0\r\n\r\n")

    # --- redirects and status grammar ---------------------------------
    for tag, data in redirect_cases():
        yield tag, data
    for tag, data in status_cases():
        yield tag, data
    for tag, data in size_overflow_cases():
        yield tag, data


def stream_cases():
    """Cases that only hang if the server keeps sending: run with fuzz.py --stream."""
    yield "stream-100-only", b"HTTP/1.1 100 Continue\r\n\r\n"
    yield "stream-100-with-hdr", b"HTTP/1.1 100 Continue\r\nX-A: b\r\n\r\n"
    yield "stream-0-only", b"HTTP/1.1 0 x\r\n\r\n"
    yield "stream-nocode-only", b"HTTP/1.1\r\n\r\n"
    yield "stream-100-4096", b"HTTP/1.1 100 " + b"x" * 4096 + b"\r\n\r\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("outdir")
    ap.add_argument("--mutate", type=int, default=0)
    ap.add_argument("--stream", action="store_true", help="emit the streaming cases")
    ap.add_argument("--seed", type=int, default=1337)
    a = ap.parse_args()
    if a.stream:
        os.makedirs(a.outdir, exist_ok=True)
        for i, (tag, data) in enumerate(stream_cases()):
            open(os.path.join(a.outdir, "%03d-%s.http" % (i, tag)), "wb").write(data)
        print("%d stream cases -> %s" % (i + 1, a.outdir))
        return 0
    if a.mutate:
        mutate_corpus(a.outdir, a.mutate, a.seed)
        return 0
    os.makedirs(a.outdir, exist_ok=True)
    n = 0
    for i, (tag, data) in enumerate(cases()):
        p = os.path.join(a.outdir, "%03d-%s.http" % (i, tag))
        with open(p, "wb") as f:
            f.write(data)
        n += 1
    print("%d cases -> %s" % (n, a.outdir))
    return 0



# ---------------------------------------------------------------------------
# mutation mode: deterministic bit/byte mutations over the chunked control
# ---------------------------------------------------------------------------
import random

SEEDS = [
    resp([b"Transfer-Encoding: chunked"], chunk(16) + chunk(16) + b"0\r\n\r\n"),
    resp([b"Transfer-Encoding: chunked", b"X-Pad: " + b"p" * 64],
         chunk(8) + b"0\r\nX-T: v\r\n\r\n"),
    resp([b"Content-Length: 32"], BODY[:32]),
    resp([b"Transfer-Encoding: chunked"], b"10\r\n" + BODY[:16] + b"\r\n0\r\n\r\n"),
]

INTERESTING = b"\x00\x01\x09\x0a\x0d\x20\x2d\x2b\x30\x31\x39\x3a\x41\x46\x61\x66\x7f\x80\xff"


def mutate(rng, base):
    d = bytearray(base)
    for _ in range(rng.randint(1, 4)):
        op = rng.randrange(6)
        if not d:
            break
        i = rng.randrange(len(d))
        if op == 0:                                  # bit flip
            d[i] ^= 1 << rng.randrange(8)
        elif op == 1:                                # interesting byte
            d[i] = INTERESTING[rng.randrange(len(INTERESTING))]
        elif op == 2:                                # delete a run
            n = rng.randint(1, 8)
            del d[i:i + n]
        elif op == 3:                                # duplicate a run
            n = rng.randint(1, 8)
            d[i:i] = d[i:i + n]
        elif op == 4:                                # insert a run of one byte
            d[i:i] = bytes([d[i]]) * rng.randint(1, 16)
        else:                                        # truncate
            del d[i:]
    return bytes(d)


def mutate_corpus(outdir, count, seed):
    rng = random.Random(seed)
    os.makedirs(outdir, exist_ok=True)
    for i in range(count):
        base = SEEDS[rng.randrange(len(SEEDS))]
        with open(os.path.join(outdir, "m%04d.http" % i), "wb") as f:
            f.write(mutate(rng, base))
    print("%d mutations (seed %d) -> %s" % (count, seed, outdir))



# ---------------------------------------------------------------------------
# redirect / status / framing grammar (dimensions random mutation cannot reach)
# ---------------------------------------------------------------------------
def redirect_cases():
    TE = b"Transfer-Encoding: chunked"
    ok = resp([TE], chunk(8) + b"0\r\n\r\n")
    for tag, loc in [
        ("loc-query-only", b"?x"),
        ("loc-frag-only", b"#x"),
        ("loc-query-slash", b"?x/y"),
        ("loc-frag-slash", b"#x/y"),
        ("loc-query-empty", b"?"),
        ("loc-frag-empty", b"#"),
        ("loc-query-at", b"?a@b"),
        ("loc-frag-at", b"#a@b"),
        ("loc-query-pct", b"?%41%42"),
        ("loc-query-pct-trunc", b"?%4"),
        ("loc-query-pct-nul", b"?%00x"),
        ("loc-query-pct-slash", b"?%2f%2f"),
        ("loc-abs-path", b"/x"),
        ("loc-proto-rel", b"//127.0.0.1:1/x"),
        ("loc-scheme-http", b"http://127.0.0.1:1/x"),
        ("loc-scheme-junk", b"zzz://h/x"),
        ("loc-no-scheme-host", b"127.0.0.1:1/x"),
        ("loc-user-at", b"http://u:p@127.0.0.1:1/x"),
        ("loc-user-pct", b"http://u%41:p%20x@127.0.0.1:1/x"),
        ("loc-user-pct-trunc", b"http://u%4@127.0.0.1:1/x"),
        ("loc-8192", b"/" + b"y" * 8192),
        ("loc-8192-query", b"?" + b"y" * 8192),
        ("loc-empty", b""),
        ("loc-space", b" "),
        ("loc-tab", b"\t"),
        ("loc-4096", b"?" + b"y" * 4096),
    ]:
        yield tag, resp([b"Location: " + loc, b"Content-Length: 0"], b"", b"HTTP/1.1 302 Found")
    # a redirect that lands back on the same server and succeeds
    yield "loc-self-ok", resp([b"Location: /x", b"Content-Length: 0"], b"", b"HTTP/1.1 302 Found") + ok
    # redirect loops
    yield "loc-loop-self", resp([b"Location: /x", b"Content-Length: 0"], b"", b"HTTP/1.1 302 Found") * 40
    yield "loc-loop-query", resp([b"Location: ?x", b"Content-Length: 0"], b"", b"HTTP/1.1 302 Found") * 40


def status_cases():
    TE = b"Transfer-Encoding: chunked"
    ok = resp([TE], chunk(8) + b"0\r\n\r\n")
    yield "status-100-x100", resp([], b"", b"HTTP/1.1 100 Continue") * 100 + ok
    yield "status-0-x100", resp([], b"", b"HTTP/1.1 0 x") * 100 + ok
    yield "status-nocode-x100", resp([], b"", b"HTTP/1.1") * 100 + ok
    yield "status-100-with-hdrs", (resp([b"X-A: b"], b"", b"HTTP/1.1 100 Continue") * 50) + ok
    yield "status-100-then-eof", resp([], b"", b"HTTP/1.1 100 Continue") * 50
    yield "status-199", resp([], b"body", b"HTTP/1.1 199 x")
    yield "status-204", resp([], b"", b"HTTP/1.1 204 No Content")
    yield "status-206", resp([], b"body", b"HTTP/1.1 206 Partial")
    yield "status-304", resp([], b"", b"HTTP/1.1 304 Not Modified")
    yield "status-400", resp([], b"", b"HTTP/1.1 400 Bad")
    yield "status-999", resp([], b"", b"HTTP/1.1 999 x")
    yield "status-neg", resp([], b"", b"HTTP/1.1 -1 x")
    yield "status-plus", resp([], b"", b"HTTP/1.1 +200 x")
    yield "status-hex", resp([], b"", b"HTTP/1.1 0xc8 x")
    yield "status-huge", resp([], b"", b"HTTP/1.1 99999999999999999999 x")


# ---------------------------------------------------------------------------
# chunk-size values that overflow the parser (strtol clamps to LONG_MAX)
# ---------------------------------------------------------------------------
def size_overflow_cases():
    TE = b"Transfer-Encoding: chunked"
    for tag, line, n in [
        ("ovf-ffffffff", b"ffffffff", 70000),
        ("ovf-ffffffff00010000", b"ffffffff00010000", 70000),
        ("ovf-100000000", b"100000000", 70000),
        ("ovf-7fffffff", b"7fffffff", 70000),
        ("ovf-80000000", b"80000000", 70000),
        ("ovf-ffffffff0001f400", b"ffffffff0001f400", 128000),
        ("ovf-ffffffff00100000", b"ffffffff00100000", 300000),
        ("ovf-0x-prefixed", b"0xffffffff", 70000),
        ("ovf-neg-sign", b"-1", 70000),
        ("ovf-plus-huge", b"+ffffffff", 70000),
    ]:
        yield tag, resp([TE], line + b"\r\n" + b"A" * n + b"\r\n0\r\n\r\n")
if __name__ == "__main__":
    sys.exit(main())
