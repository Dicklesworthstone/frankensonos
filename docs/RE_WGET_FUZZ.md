# RE_WGET_FUZZ — busybox wget chunked-grammar fuzz (bead frankensonos-s20)

Lane: the 57.23 updater fetcher is `/usr/bin/wget` (busybox), exec'd as root via `execl`
**pre-signature** (ROOT_LANE_STATUS 2026-10-24 00:30Z). A malicious LAN update URL
(`BeginSoftwareUpdate`) reaches this parser before any verification, so a grammar flaw here is a
pre-auth, root-privileging primitive.

Everything below is offline. No device run is in this file; the proposed device runs are at the end
and need the orchestrator's approval.

Two harnesses live in `recon/wgetfuzz/`, written by two panes:

| harness | shape | use it for |
|---|---|---|
| `wgetfuzz.py` + `resp_server.py` (PearlMink) | JSONL corpus, `nc -l` server, `--gen-corpus` / `--corpus` | the 44-case first pass, `cases.jsonl`, `faults/` |
| `chunk_gen.py` + `chunk_srv.py` + `chunk_fuzz.py` (RusticBarn) | one `.http` file per case, byte-exact server, SEGV/ABORT/HANG/DIE/OK classifier, `--stream`, `--mutate` | the 2 139-case sweep below, the streaming cases, redirect chains |

## The target, extracted

- Binary: `captures/firmware/57.23-74170-1-16/rootfs-extracted/bin/busybox`
  (288 488 B, ELF 32-bit MSB PPC, dynamically linked, stripped; sha256
  `989ec0daad0778a8d66e6c8ddbac4dc25bfdf73a40c5ea5ebd01d6ded28749b2`).
- Version string: **BusyBox v1.24.2** (no build date in the parens, no config string).
- `/usr/bin/wget -> ../../bin/busybox` in the same rootfs.
- Deps: `ld.so.1 libc.so.6 libcrypt.so.1 libdl.so.2 libm.so.6 libgcc_s.so.1`, all present in the
  extracted `/lib` (checksums verified after copy).
- Has `chunked`/`transfer-encoding` strings, i.e. the dechunker is compiled in.

Correction to `RE_CVES.md` §1: the busybox row there (`v1.00` / `v1.14.1`) comes from the **34.16**
trees (`s1_rootfs`, `play1_rootfs`). The 57.23 model-16 build is **1.24.2**, and it is the one the
updater runs.

## The updater's real argv — `-C` takes the report path

`RE_WRITE_PRIMITIVES.md` records the execl as `wget -qC -T 300 -O - <url>`. The strings are right
but the order is not: `-C` is an option **with an argument** in this build (`getopt` string
`csqO:P:Y:U:T:C:t:` at `0x10041614`), so `-qC -T 300 -O -` makes `-T` the argument of `-C`, `300`
the URL, and the fetch goes to `0.0.1.44`. The real call is at `0x10009060` (`bl 0x1002bf64`):

```
r3 0x10018814 "/usr/bin/wget"      r7  0x10018808 "-T"
r4 0x100187fc "wget"               r8  0x1001880c "300"
r5 0x10018804 "-qC"                r9  0x10018810 "-O"
r6 0x100187e0 "/var/run/upgrade_report.txt"   r10 0x10017b88 "-"
stack+8 <url>                      stack+12 NULL
```

So: `wget -qC /var/run/upgrade_report.txt -T 300 -O - <url>`. `-C` opens a small report file and
writes `Server:`, `ServerIP:`, `Content-length:` into it (`0x10011114`, `0x100111a4`, `0x100119a8`);
it is not the stock `-C`/`--no-cache` flag. A harness that omits the report path is testing a
different invocation.

## Offline rig (css)

`~/fsonos-s20/` holds the busybox + its own libs; `qemu-ppc` (10.2.1, Ubuntu on css) runs the
applet. The runner hides `/etc/ld.so.cache` per run — see the environment fact below — serves a
canned response, and fires the device argv.

### Environment fact: guest ld.so vs host ld.so.cache

The guest's 2.x-era `ld.so` **segfaults** when the host's modern `glibc-ld.so.cache1.1` file is
visible: it misparses the cache header and computes an entry count of `0x1705ffff` (measured in gdb
at the loop `0x40818164`–`0x408181b0` in the mapped ld.so: `r29 = *(cache+20)-1` straight from the
magic block bytes, then `mulli r31,24` indexes to `0x54cb6fe8`, unmapped). Symptom:
`qemu: uncaught target signal 11` before any applet code runs; the busybox banner never prints.
Hiding `/etc/ld.so.cache` (or shipping an empty one in the sysroot) fixes it completely.

## Results

### First pass (PearlMink, 2026-10-27): 44 cases, 0 signals

`recon/wgetfuzz/cases.jsonl`, regenerate with `wgetfuzz.py --gen-corpus`, response bytes saved for
every non-clean case under `recon/wgetfuzz/faults/`. 34 CLEAN, 10 `WAIT_SHORT_BODY`.

| family | cases | verdict |
|---|---|---|
| valid baseline | 1 | CLEAN, dechunked bytes correct |
| zero-chunk-then-more (the live "ends body early" note) | 1 | CLEAN, rc 0 |
| huge declared size, short/truncated data | 2 | WAIT_SHORT_BODY |
| leading zeros / spaces / lowercase-mixed sizes | 3 | CLEAN / WAIT |
| 64-bit size, 9-digit size, bit31 size | 3 | WAIT_SHORT_BODY |
| garbage/`+`/`-`/empty/CR-only/very-long sizes | 6 | CLEAN (all rejected or parsed) |
| chunk extensions (long, quoted, many, unbalanced quote) | 8 | CLEAN |
| trailers (simple, 500 headers, 60 KiB value) | 3 | CLEAN |
| bare-LF endings, CRLF inside data, NUL in size/headers | 3 | CLEAN |
| 5 000 tiny chunks; 64 KiB one-header-per-byte | 2 | CLEAN |
| CL+TE together; TE duplicated | 2 | CLEAN |
| 1 MiB status line; 1 MiB header value | 2 | CLEAN |
| gzip-marked chunked; HTTP/1.0 chunked; no reason phrase; EOF shapes | 6 | CLEAN / WAIT |

`WAIT_SHORT_BODY`: wget writes every byte that arrived and then waits for the rest of the declared
chunk until the timeout kills it — a hang on an under-delivered body, not a fault.

### Second pass (RusticBarn, 2026-10-27): 2 139 cases, 0 faults

| sweep | cases | SEGV | ABORT | HANG | DIE | OK |
|---|---|---|---|---|---|---|
| grammar (status, headers, chunk size, terminators, trailers, TE/CL grammar, redirects, size overflow) | 134 | 0 | 0 | 0 | 82 | 52 |
| random mutation, seed 1337 (bit flips, interesting bytes, delete/duplicate/insert runs, truncation) | 2 000 | 0 | 0 | 0 | 1 085 | 915 |
| streaming (`100` only, `100`+header, `0` only, no code, 4 KiB `100`) | 5 | 0 | 0 | **4** | 1 | 0 |

`DIE` is the intended `bb_error_msg_and_die` path, not a fault. Every mutation that survived the
header block produced a clean error or a clean exit.

### Why the obvious overflow is not reachable here

Upstream fixed a chunk-length bug in 1.29.0 (`8e2174e9bd83`, *"Had a bug with inputs like
`ffffffff0001f400` smashing the heap later"*): `fread(G.wget_buf, 1, rdsz, dfp)` with `rdsz` taken
from the chunk size, where `G.wget_buf` is the **last member** of the heap-allocated `G` struct.

That needs `rdsz > sizeof(G.wget_buf)`. On this build it cannot happen:

- `G` is one `xzalloc(0x1040)` — 4 160 B, `wget_buf` at `+0x40` and the **last** member, so the
  buffer ends exactly at the end of the allocation (`li r3, 0x1040` at `0x10010e4c`, immediately
  before the `getopt32` setup). The buffer itself is 4 096 B (`CONFIG_FEATURE_COPYBUF_KB=4`;
  measured — a status line fits to 4 093 characters and the remainder is parsed as a header);
- `off_t` is 32-bit (LFS off) and `STRTOOFF` is `strtol` (`include/libbb.h`, `ENABLE_LFS` off), so
  the chunk size is clamped to `LONG_MAX` with `ERANGE`. The parse is `li 5, 16` / `bl strtol` /
  `stw 3, 0(29)` at `0x10010dcc`–`0x10010ddc` — a 32-bit store, so `content_len` is 4 bytes;
- `content_len` is therefore always in `[0, LONG_MAX]`, and `rdsz = sizeof(G.wget_buf)` whenever
  `content_len >= 4096`.

Measured, `ffffffff0001f400` with 128 000 bytes of body: every `write(1, …)` is 4 096 B, no fault.
`ffffffff`, `100000000`, `7fffffff`, `80000000`, `0xffffffff` and `+ffffffff` behave identically.
A negative value (`-1`) is the only one that changes shape: it breaks out of the read loop and the
body is re-parsed as chunk-size lines (61 819 of 70 000 bytes emitted, then
`error getting response`).

## Finding 1 — an endless `100`/`0` status stream hangs the fetcher forever

`read_response` (status line read at `0x10011434`, source `networking/wget.c:1042`, the
`case 0: case 100:` arm at `:1052`) treats status `0` and `100` as "eat the headers and read another
response line". A server that keeps answering `HTTP/1.1 100 Continue\r\n\r\n` keeps wget in that
loop indefinitely.

`-T` does not stop it. The alarm is armed once, around `connect`, and the handler only dies while
`G.connecting` is set:

```
100105a0  bl alarm            ; alarm(G.timeout_seconds)
100105a4  li 0,1
100105a8  stb 0,36(29)        ; G.connecting = 1
100105b0  bl xconnect_stream
100105bc  stb 0,36(29)        ; G.connecting = 0
```

Measured: `-T 3` against a server streaming `HTTP/1.1 100 Continue\r\n\r\n` — still running at 8 s
and at 20 s (`timeout(1)` rc 124 both times). Upstream fixed exactly this in 1.29.0
(`3459024bf404`) by re-arming the alarm on every line read (`set_alarm()` at the top of
`fgets_trim_sanitize`).

Impact: the updater waits on the child, so the update neither completes nor errors — the unit stays
in the update state until something else reboots it. Pre-auth, no memory corruption.

## Finding 2 — 1-byte heap underflow on a scheme-less `?`/`#` Location

`parse_url` (`0x100108xx`) for a URL whose host part starts with `?` or `#`:

```
10010900  mr   4,28           ; src = host
10010904  addi 3,28,-1        ; dst = host - 1
10010908  sub  5,29,28        ; len = sp - host   (0 here)
1001090c  bl   memmove
10010918  addi 9,9,-1
1001091c  stw  9,16(30)       ; host -= 1
10010920  stb  0,-1(29)       ; sp[-1] = 0
```

`host` is the start of the `xstrdup`'d URL, so `sp[-1] = 0` writes **one byte before the
allocation** — the low byte of the dlmalloc size field on big-endian. The same instruction also
zeroes the first byte of the host, so the host becomes `""` and the fetch dies.

Reachable from the wire with a single redirect: `HTTP/1.1 302` + `Location: ?x` (or `#x`, `?`, `#`,
`?x/y`, `?%41`, `?a@b`, …). Measured, all of them: `wget: bad address ''`.

Upstream fixed it in 1.29.0 (`df45eb49acae`, *"we used to underflow the allocated space"*), replacing
the three instructions with `*sp++ = '\0'; h->path = sp;`.

Impact today: functional — a server that redirects to a query-only URL kills the fetch. The
corrupted chunk is never freed on this path because the process exits at `bad address`, so the
underflow itself is not observable as a crash.

## Finding 3 — server data is not sanitized

1.29.0 (`3459024bf404`) added `sanitize_string`/`get_sanitized_hdr` over every line the other side
sends, because control characters reach `parse_url` and the error messages. On 1.24.2 the only
trimming is `\r`/`\n` in `fgets_and_trim`.

Measured: `Location: /x\x01y` produces the request line `GET /x^Ay HTTP/1.1` — the control byte is
forwarded verbatim. A `\x00` truncates the Location (the string ends there). `\x1b` and `\xff`
survive into the path. No CRLF injection is possible because `\r`/`\n` are trimmed.

## Not a bug here

- `Content-Length` is validated (`< 0 || errno` → die) and cannot overflow the buffer.
- `fgets` is bounded (`sizeof(G.wget_buf) - 1`) and a truncated line is drained by `getc`, so a long
  header or status line is truncated, not overrun. A truncated status line leaves its tail to be
  parsed as a header and dies with `bad header line` — a clean error.
- `percent_decode_in_place` only ever shrinks and is bounded by the source.
- Chunk terminators, trailers, empty lines and `0`-length chunks are all tolerated.

## Interpretation for the lane

- The dechunker is length-driven and robust across every axis probed: it never scans past a declared
  size, and garbage sizes are rejected without a crash.
- The live `~128 KiB` fetch abort (ROOT_LANE_STATUS 2026-10-24 00:30Z) did **not** reproduce in the
  wget parser offline, with a 1 MiB status line or with a 1 MiB header value — that gate is
  elsewhere in the updater chain (or is a socket/buffer limit in the execl'd context).
- No fault ⇒ the "chunked decoder deserves a real fuzz" hypothesis is answered negative on the
  offline side. The falsifier branch of the bead ("corpus executes clean") is satisfied; device
  verdicts are the remaining half and are gated on coordinator approval.

## Proposed device runs (need approval)

The updater is the only consumer, so a device verdict means serving a crafted `.upd` and firing
`ZoneGroupTopology#BeginSoftwareUpdate` at `.202` (57.23, the S1 unit already used for the poison
runs). One experiment per unit per minute; the version is expected to stay `57.23-74170` because the
signature is stale in every case.

| # | shape | predicted | discriminates |
|---|---|---|---|
| D1 | genuine package, served with `Transfer-Encoding: chunked` and one chunk per 8 KiB | full read, `No valid signature` | chunked is accepted at all on the device (control) |
| D2 | D1 but the response is `HTTP/1.1 100 Continue` forever | fetch never ends, no abort, no version change | finding 1 on hardware |
| D3 | D1 but a `302` with `Location: ?x` first | `bad address ''`, fetch ends, daemon restarts | finding 2 on hardware |

D1 is the control that makes D2/D3 meaningful; without it a "hang" could be the stale-URL loop
already documented in `ROOT_LANE_STATUS.md`.

## Reuse

```
python3 recon/wgetfuzz/wgetfuzz.py --gen-corpus --corpus recon/wgetfuzz/cases.jsonl
python3 recon/wgetfuzz/wgetfuzz.py --host css --corpus recon/wgetfuzz/cases.jsonl --out recon/wgetfuzz/results.jsonl
python3 recon/wgetfuzz/wgetfuzz.py --host css --only baseline_valid,size_bit31
```

```
python3 recon/wgetfuzz/chunk_gen.py cases                       # 134 grammar cases
python3 recon/wgetfuzz/chunk_gen.py mut --mutate 2000 --seed 1337
python3 recon/wgetfuzz/chunk_gen.py stream --stream
python3 recon/wgetfuzz/chunk_fuzz.py --root ~/wgetfuzz/root --cases ~/wgetfuzz/cases --out run.jsonl
python3 recon/wgetfuzz/chunk_fuzz.py --stream ...               # with the stream corpus
```

Prereq (one-time): copy `rootfs-extracted/bin/busybox` + the six `/lib` libs to
`css:~/fsonos-s20/{bin,lib}` (or `~/wgetfuzz/root/{bin,lib}` for `chunk_fuzz.py`); qemu-ppc on css.
`chunk_fuzz.py` uses the device argv with `-T 5` and a 5 s `timeout(1)` wrapper.
