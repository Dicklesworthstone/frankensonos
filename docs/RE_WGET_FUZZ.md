# RE_WGET_FUZZ — busybox wget chunked-grammar fuzz (bead frankensonos-s20)

Lane: the 57.23 updater fetcher is `/usr/bin/wget` (busybox, symlink) exec'd as
root via execl **pre-signature** (ROOT_LANE_STATUS 2026-10-24 00:30Z). A
malicious LAN update URL (BeginSoftwareUpdate, attacker-chosen) reaches this
parser before any verification, so a grammar flaw here is a root-privileging
pre-auth primitive.

## The target, extracted

- Binary: `captures/firmware/57.23-74170-1-16/rootfs-extracted/bin/busybox`
  (288,488 bytes, ELF 32-bit MSB PPC, dynamically linked, stripped).
- Version string: **BusyBox v1.24.2** (no build date in the parens).
- `/usr/bin/wget -> ../../bin/busybox` in the same rootfs.
- Deps: `ld.so.1 libc.so.6 libcrypt.so.1 libdl.so.2 libm.so.6 libgcc_s.so.1`,
  all present in the extracted `/lib` (checksums verified after copy).
- Has `chunked`/`transfer-encoding` strings, i.e. the dechunker is compiled in.

## Offline rig (css)

`~/fsonos-s20/` holds the busybox + its own libs; `qemu-ppc` (10.2.1, Ubuntu on
css) runs the applet. The runner (`recon/wgetfuzz/wgetfuzz.py`) hides
`/etc/ld.so.cache` per run — see the environment fact below — serves a canned
response with `nc -l`, and fires `wget -O out.bin http://127.0.0.1:8321/f`.

### Environment fact: guest ld.so vs host ld.so.cache

The guest's 2.x-era `ld.so` **segfaults** when the host's modern
`glibc-ld.so.cache1.1` file is visible: it misparses the cache header and
computes an entry count of `0x1705ffff` (measured in gdb at the loop
0x40818164–0x408181b0 in the mapped ld.so: `r29 = *(cache+20)-1` straight from
the magic block bytes, then `mulli r31,24` indexes to `0x54cb6fe8`, unmapped).
Symptom: `qemu: uncaught target signal 11` before any applet code runs; the
busybox banner never prints. Hiding `/etc/ld.so.cache` (the runner does
`mv`/restore around every run) fixes it completely.

## Corpus and results (2026-10-27, full run)

44 deterministic cases (`recon/wgetfuzz/cases.jsonl`, regenerate with
`wgetfuzz.py --gen-corpus`), one connection per case, response bytes saved for
every non-clean case under `recon/wgetfuzz/faults/`.

Verdict: **0 signals** (no SIGSEGV/SIGABRT/SIGFPE, no daemon-class fault
oracle exists offline — this is the wget process itself). 34 CLEAN, 10
`WAIT_SHORT_BODY`.

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
| 5,000 tiny chunks; 64 KiB one-header-per-byte | 2 | CLEAN |
| CL+TE together; TE duplicated | 2 | CLEAN |
| 1 MiB status line; 1 MiB header value | 2 | CLEAN (the live ~128 KiB abort did not reproduce offline — that gate is elsewhere in the updater, not in wget's parser) |
| gzip-marked chunked; HTTP/1.0 chunked; no reason phrase; EOF shapes | 6 | CLEAN / WAIT |

`WAIT_SHORT_BODY` semantics, verified on `size_mixed_case` (declared `aBcD` =
43,981 bytes, 188 served): wget writes **every byte that arrived** to the
output file and then waits for the remainder of the declared chunk until
`timeout` kills it. A hang on an under-delivered body, not a fault and not a
corrupted prefix.

## Interpretation for the lane

- The dechunker is length-driven and robust across the grammar axes probed:
  it never scans past a declared size (case 14: CRLF inside data, CLEAN), and
  garbage sizes are rejected without a crash (rc 1, `server returned error`).
- The live `~128 KiB` fetch abort (ROOT_LANE_STATUS 2026-10-24 00:30Z) did
  **not** reproduce in the wget parser offline with a 1 MiB status line — the
  gate is elsewhere in the updater chain (or is a socket/buffer limit in the
  execl'd context), worth one on-device confirm later.
- No fault ⇒ the "chunked decoder deserves a real fuzz" hypothesis is, for
  this grammar surface, answered negative on the offline side. The falsifier
  branch of the bead ("corpus executes clean") is halfway satisfied; device
  verdicts are the remaining half and are gated on coordinator approval.

## Reuse

```
python3 recon/wgetfuzz/wgetfuzz.py --gen-corpus --corpus recon/wgetfuzz/cases.jsonl
python3 recon/wgetfuzz/wgetfuzz.py --host css --corpus recon/wgetfuzz/cases.jsonl --out recon/wgetfuzz/results.jsonl
python3 recon/wgetfuzz/wgetfuzz.py --host css --only baseline_valid,size_bit31
```

Prereq (one-time): copy `rootfs-extracted/bin/busybox` + the six `/lib` libs
to `css:~/fsonos-s20/{bin,lib}`; qemu-ppc on css.
