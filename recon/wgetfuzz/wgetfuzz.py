#!/usr/bin/env python3
"""Chunked-encoding grammar fuzzer for the device's own busybox wget (s20).

Target: /usr/bin/wget -> busybox 1.24.2 (PPC 32-bit BE, dynamically linked),
the binary the 57.23 updater execs as root pre-signature
(ROOT_LANE_STATUS 2026-10-24 00:30Z: the fetcher is /usr/bin/wget busybox,
execl'd as root; a 1 MiB status line aborts at ~128 KiB, a zero chunk ends the
body early).

This harness runs OFFLINE on css: qemu-ppc + the extracted busybox + its own
/lib. A deterministic corpus of chunked-grammar cases (sizes, extensions,
trailers, obfuscations) is fired at wget one case at a time; every run is
classified by exit signal and a core/qemu log is kept on any anomaly.

Important environment fact (measured 2026-10-26): the host /etc/ld.so.cache
must be hidden while qemu-ppc runs this guest, otherwise the guest's old
ld.so (2.x, 2015-era) misparses the modern cache format and segfaults in
ld.so before the applet runs. The runner does the move/restore itself.

usage:
  ./wgetfuzz.py --host css --corpus cases.jsonl [--out results.jsonl]
  python3 wgetfuzz.py --gen-corpus cases.jsonl       # regenerate corpus
"""
import argparse
import json
import os
import subprocess
import sys
import time

REMOTE_DIR = 'fsonos-s20'
BB = f'~/{REMOTE_DIR}/bin/busybox'
PREFIX = f'~/{REMOTE_DIR}'
PORT = 8321

HOST_HIDE_CACHE = 'sudo -n mv /etc/ld.so.cache /tmp/ld.so.cache.host'
HOST_RESTORE_CACHE = 'sudo -n mv /tmp/ld.so.cache.host /etc/ld.so.cache'


def ssh(host, cmd, timeout=60):
    return subprocess.run(['ssh', host, cmd], capture_output=True, text=True,
                          timeout=timeout)


def gen_cases():
    """Deterministic chunked-grammar corpus. Each case: (name, raw response bytes)."""
    cases = []

    def add(name, resp):
        cases.append({'name': name, 'resp_hex': resp.hex()})

    def chunk(size_hex, data, ext=''):
        line = size_hex if isinstance(size_hex, bytes) else size_hex.encode()
        line = line + f'{ext}\r\n'.encode()
        return line + data + b'\r\n'

    body = b'hello world, this is segment data for the fuzz\n' * 4

    # 1. baseline: valid chunked
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + chunk(b'o', b' world')
            + b'0\r\n\r\n')
    add('baseline_valid', resp)

    # 2. zero chunk early, then more data after it (observed live: ends body early)
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + b'0\r\n\r\n' + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('zero_chunk_then_more', resp)

    # 3. huge declared size, short data
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'FFFFFFFF', body[:8]) + b'0\r\n\r\n')
    add('huge_size_short_data', resp)

    # 4. huge declared size, connection just ends (truncated mid-chunk)
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'7FFFFFFF', body[:16]))
    add('huge_size_truncated', resp)

    # 5. size with leading zeros and spaces
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'00000004', b'hell') + b'0\r\n\r\n')
    add('leading_zero_size', resp)

    # 6. size with whitespace before CRLF
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4  ', b'hell') + b'0\r\n\r\n')
    add('space_after_size', resp)

    # 7. chunk extension forms
    for name, ext in [
        ('ext_semicolon', ';'),
        ('ext_name', ';name=v'),
        ('ext_quoted', ';q="a;b"'),
        ('ext_very_long', ';' + 'x' * 4096),
        ('ext_many', ';' + ';'.join(f'k{i}=v{i}' for i in range(200))),
        ('ext_equals_only', ';='),
        ('ext_empty_quoted', ';q=""'),
        ('ext_quote_unbalanced', ';q="unclosed'),
    ]:
        resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
                + chunk(b'4', b'hell', ext) + b'0\r\n\r\n')
        add(name, resp)

    # 8. negative-looking size (huge hex => big positive)
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'80000000', body[:16]) + b'0\r\n\r\n')
    add('size_bit31', resp)

    # 9. size overflow: 16 hex digits
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'FFFFFFFFFFFFFFFF', body[:16]) + b'0\r\n\r\n')
    add('size_64bit', resp)

    # 10. size non-hex garbage
    for name, s in [('size_garbage', b'zz'), ('size_plus', b'+4'),
                    ('size_minus', b'-4'), ('size_empty', b''),
                    ('size_only_cr', b'\r'), ('size_very_long', b'1' * 128)]:
        resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
                + chunk(s, b'hell') + b'0\r\n\r\n')
        add(name, resp)

    # 11. trailers
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell')
            + b'0\r\nX-Trailer: v\r\n\r\n')
    add('trailer_simple', resp)

    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell')
            + b'0\r\n' + b''.join(f'X-T{i}: v{i}\r\n'.encode() for i in range(500))
            + b'\r\n')
    add('trailer_500_headers', resp)

    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell')
            + b'0\r\n' + (b'X-Big: ' + b'v' * 60000 + b'\r\n\r\n'))
    add('trailer_60k_value', resp)

    # 12. no final CRLF after zero chunk
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n')
    add('zero_chunk_no_final_crlf', resp)

    # 13. bare LF line endings everywhere
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            .replace(b'\r\n\r\n', b'\n\n')
            + chunk(b'4', b'hell').replace(b'\r\n', b'\n')
            + b'0\n\n')
    add('bare_lf_endings', resp)

    # 14. chunk data containing CR/LF (parser must use size, not scan)
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'10', b'abc\r\nGET / HTTP/1.1\r\n') + b'0\r\n\r\n')
    add('crlf_inside_data', resp)

    # 15. many tiny chunks
    many = b''.join(chunk(b'1', bytes([65 + (i % 26)])) for i in range(5000))
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + many + b'0\r\n\r\n')
    add('many_tiny_chunks_5000', resp)

    # 16. chunk size 1 with 64 KiB total data, one byte per chunk header
    # (header-heavy stress)
    many = b''.join(chunk(b'1', b'A') for i in range(65536))
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + many + b'0\r\n\r\n')
    add('header_per_byte_64k', resp)

    # 17. TE both headers (Content-Length + chunked)
    resp = (b'HTTP/1.1 200 OK\r\nContent-Length: 5\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('cl_plus_te', resp)

    # 18. TE: chunked, chunked (duplicate header)
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('te_duplicate', resp)

    # 19. 1 MiB status line (ROOT_LANE_STATUS: aborts ~128 KiB live)
    resp = (b'HTTP/1.1 200 ' + b'S' * (1 << 20) + b'\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('status_line_1mib', resp)

    # 20. 1 MiB single header value
    resp = (b'HTTP/1.1 200 OK\r\nX-Big: ' + b'V' * (1 << 20) + b'\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('header_value_1mib', resp)

    # 21. gzip-style Content-Encoding with chunked (decoder interplay)
    resp = (b'HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'\x1f\x8b\x08\x00') + b'0\r\n\r\n')
    add('gzip_marked_chunked', resp)

    # 22. dechunk + content-length mismatch games
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'0FFFFFFFF', body[:16]) + b'0\r\n\r\n')
    add('size_9_digits', resp)

    # 23. last-chunk with size '0' and extensions
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0;last\r\n\r\n')
    add('zero_chunk_extension', resp)

    # 24. lowercase hex / mixed
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'aBcD', body[:0xabcd if 0xabcd < len(body) else len(body)])
            + b'0\r\n\r\n')
    add('size_mixed_case', resp)

    # 25. data longer than declared size (next bytes are not a size)
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hellworld longer') + b'0\r\n\r\n')
    add('data_longer_than_size', resp)

    # 26. size line without CRLF at EOF
    resp = (b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4')
    add('size_only_eof', resp)

    # 27. immediate EOF after headers
    resp = b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
    add('eof_after_headers', resp)

    # 28. HTTP/1.0 + chunked (spec says 1.0 has no chunked)
    resp = (b'HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('http10_chunked', resp)

    # 29. no reason phrase
    resp = (b'HTTP/1.1 200\r\nTransfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4', b'hell') + b'0\r\n\r\n')
    add('no_reason_phrase', resp)

    # 30. NUL bytes in size line and headers
    resp = (b'HTTP/1.1 200 OK\r\nX-N: a\x00b\r\n'
            b'Transfer-Encoding: chunked\r\n\r\n'
            + chunk(b'4\x00', b'hell') + b'0\r\n\r\n')
    add('nul_in_size_and_headers', resp)

    return cases


def classify(rc, stderr):
    """rc: negative = signal (as in bash), or qemu's 139 for SIGSEGV passthrough.

    RC124 (timeout, measured on the mixed-case run 2026-10-27) is the
    declared-size >> actual-data shape: busybox 1.24.2 wget waits for the
    remaining bytes of an under-delivered chunk until timeout, having written
    everything that arrived (out.bin holds all served body bytes, not a
    corrupted prefix). It is a hang-on-short-body, not a fault; classified
    separately so a real signal stands out.
    """
    if rc in (0, 1, 4, 5):
        return 'CLEAN'   # wget's own error codes (1 = server err, 4/5 network fail)
    if rc == 124:
        return 'WAIT_SHORT_BODY'
    if rc in (139, -11):
        return 'SIGSEGV'
    if rc in (134, -6):
        return 'SIGABRT'
    if rc in (136, -8):
        return 'SIGFPE'
    if rc in (137, -9):
        return 'SIGKILL'
    return f'RC{rc}'


def run_case(host, name, resp, outdir):
    with open(f'/tmp/wf_{name}.bin', 'wb') as fh:
        fh.write(resp)
    subprocess.run(['scp', '-q', f'/tmp/wf_{name}.bin',
                    f'{host}:{REMOTE_DIR}/resp.bin'], timeout=30)
    # serve the raw response, then wget it. nc -l serves resp.bin once.
    cmd = (
        f'cd ~/{REMOTE_DIR} && '
        f'({HOST_HIDE_CACHE}; '
        f'(nc -l -p {PORT} < resp.bin &) ; '
        f'sleep 0.4; '
        f'rm -f out.bin; '
        f'timeout 30 qemu-ppc -L ~/{REMOTE_DIR} {BB} wget -O out.bin '
        f'http://127.0.0.1:{PORT}/f 2>/tmp/wf_err.txt; echo RC=$?; '
        f'ls -la out.bin 2>/dev/null; '
        f'{HOST_RESTORE_CACHE}) 2>&1 | tail -4'
    )
    p = ssh(host, cmd, timeout=90)
    out = p.stdout.strip()
    rc = -999
    for line in out.splitlines():
        if line.startswith('RC='):
            try:
                rc = int(line[3:])
            except ValueError:
                pass
    stderr = ''
    se = ssh(host, 'cat /tmp/wf_err.txt', timeout=15)
    stderr = se.stdout.strip()
    cls = classify(rc, stderr)
    # keep the response bytes for any non-clean case
    if cls not in ("CLEAN", "WAIT_SHORT_BODY"):
        os.makedirs(outdir, exist_ok=True)
        with open(os.path.join(outdir, f'{name}.resp.bin'), 'wb') as fh:
            fh.write(resp)
        ssh(host, f'cp /tmp/wf_err.txt ~/{REMOTE_DIR}/{name}.err', timeout=15)
    return {'name': name, 'rc': rc, 'class': cls, 'stderr': stderr[-400:]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--host', default='css')
    ap.add_argument('--corpus', default='cases.jsonl')
    ap.add_argument('--out', default='results.jsonl')
    ap.add_argument('--gen-corpus', action='store_true')
    ap.add_argument('--only', default=None, help='comma list of case names')
    args = ap.parse_args()

    if args.gen_corpus:
        cases = gen_cases()
        with open(args.corpus, 'w') as fh:
            for c in cases:
                fh.write(json.dumps(c) + '\n')
        print(f'wrote {len(cases)} cases to {args.corpus}')
        return 0

    with open(args.corpus) as fh:
        cases = [json.loads(l) for l in fh if l.strip()]
    if args.only:
        want = set(args.only.split(','))
        cases = [c for c in cases if c['name'] in want]

    # one-time setup: push busybox + libs (idempotent)
    results = []
    for i, c in enumerate(cases):
        resp = bytes.fromhex(c['resp_hex'])
        r = run_case(args.host, c['name'], resp, 'faults')
        results.append(r)
        print(f"[{i+1}/{len(cases)}] {r['name']:<28} rc={r['rc']:<6} {r['class']}")
        sys.stdout.flush()
        with open(args.out, 'w') as fh:
            for rr in results:
                fh.write(json.dumps(rr) + '\n')

    bad = [r for r in results if r['class'] != 'CLEAN']
    print(f'\n{len(results)} cases, {len(bad)} non-clean')
    for r in bad:
        print(f"  {r['name']}: {r['class']} :: {r['stderr'][-120:]}")
    return 0


if __name__ == '__main__':
    sys.exit(main())
