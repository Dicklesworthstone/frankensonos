# Packet-length copies in anacapad

Static audit, no hardware. Two passes merged: one over both builds (57.23 PPC and 86.10 MIPS), one
restricted to the 57.23 daemon and driven from the route table. Every claim is an instruction at a file
offset; file offsets are `vaddr - 0x10000000` into the image named in each section.

Method, pass 1 (both builds): full disassembly (`llvm-objdump --triple=powerpc` for 57.23,
`mips-linux-gnu-objdump -m mips:16` for 86.10), every `bl memcpy` / `jal memcpy|memmove` site, then the
length argument traced to its source and the preceding 10–20 instructions read for a clamp. 491 sites on
86.10, 94 with a field-loaded length on 57.23. Ranked by "length comes from the wire and the destination
is fixed".

Method, pass 2 (57.23 only): the two copy PLT stubs (`memcpy@0x10546ad8`, `memmove@0x105465f0`), 1 534
`bl` sites; the definition of the length register `r5` resolved backwards. 1 047 are `li` constants, 211
are `mr`, and 61 load the length from a struct field. Those 61 were filtered by destination: 8 write to a
stack slot, the rest to a heap pointer or an object field.

## 1. 57.23 `0x1007b2c4` — URL path span copied with no clamp (the headline finding)

Function entry `0x1007ad60` (`stwu 1,-5344(1)`), a URL splitter: `strchr` for `/` at `0x1007ae34` and
again at `0x1007ae48`, then

```
0x1007ae54  addi 29, 1, 3136        ; buffer
0x1007ae60  sub  9, 9, 24           ; span = second_slash - url_base
0x1007ae68  addi 9, 9, 1
0x1007ae70  stw  9, 5244(1)         ; span stored
0x1007ae74  li   5, 1025
0x1007ae7c  bl   0x10546ba8         ; sonosDecodePercentEncodedStr(tail, buf, 1025)  <- clamped
...
0x1007b2b8  lwz  5, 5244(1)         ; span again
0x1007b2bc  mr   4, 18              ; src = ORIGINAL url base (r18 = r4)
0x1007b2c0  mr   3, 31              ; dst = r1+16
0x1007b2c4  bl   0x10546ad8         ; memcpy(dst, src, span)  <- no clamp
0x1007b2d8  stb  30, 16(9)          ; NUL at dst+span
```

The 1025-byte clamp is **bypassed**: the span is computed before it and reused for the `memcpy`, which
writes into a 5344-byte frame from `+16`. Any URL whose path span exceeds ~5328 bytes overruns the saved
registers and the caller's frame; the 1025-byte buffer at `+3136` is not the destination, so the clamp
gives a false sense of safety. No `cmpw`/`cmplw` on the span exists between `0x1007ae70` and the copy.

Callers (pass 2): `0x101a06b4`, `0x101a0c9c`, `0x101cc704` — i.e. the URL/path cluster
`0x101a0b70`, `0x101cc878`, `0x101ddd2c`. `0x101cc878` is also finding 2.1 below, so the same machinery
both splits the URL and copies the long value. The cluster's own callers are vtable dispatches and are
still unidentified; that is the open edge.

## 2. 57.23 unchecked copies, ranked

### 2.1 `0x1ccc9c` — long-value fetch into a 2 100-byte stack buffer

```
101ccc90  lwz  5, 4208(30)      ; len  <- obj+4208
101ccc94  mr   3, 23            ; dst  = sp+16
101ccc98  lwz  4, 4212(30)      ; src  <- obj+4212
101ccc9c  bl   memcpy
```

Function `0x101cc878`, frame 2 160. There is **no compare against `4208` anywhere in the function body**
(grepped the whole range; the only `cmplw` at `0x101cc944` is on a `strrchr` result). The buffer runs
`sp+16 … sp+2116`; saved `r21`–`r31` sit at `sp+2116…sp+2156` and LR at `sp+2164`, so anything over
2 100 bytes overwrites saved registers and the return address.

The setter `0x101cbbc8` is why this is interesting: it stores the length **only when it exceeds 1 024**
(`cmplwi 7,3,1024` at `0x101cbbbc`) and imposes no upper cap, so the field is by construction a "long
value" and the only question is how long the producer lets it get.

**Setter semantics, corrected (2026-10-26).** `r29` is the `strncasecmp` result, not the span: the code
continues only when `strncasecmp(url,"x-file-cifs://",14) == 0`, so `r29` is always **0** at the store.
The three-way branch at `0x101cbbbc` is therefore: span `== 1024` → no store; span `< 1024` → store the
span; span `> 1024` → **store 0**. So the first `memcpy` at `0x101ccc9c` is bounded by 1 023 and cannot
overflow on its own.

**The unbounded copy is the second one, `0x101cccd0`.** Immediately after, the same function does

```
101cccb4  bl   strchr            ; strchr(url + stored_len, '/')
101cccb8  addi 0, 29, -1
101cccbc  sub  27, 3, 0          ; len2 = slash - (tail - 1)
101cccc4  mr   5, 27
101ccccc  mr   3, 28             ; dst = sp+16 + stored_len
101cccd0  bl   memcpy            ; no clamp, no NULL check on the strchr result
```

`len2` is the distance from the first `/` after the stored value to the **next** `/`, plus one — i.e. the
length of the first path segment, straight off the wire, with no bound. A missing second `/` makes
`strchr` return NULL and `len2` becomes `-(tail-1)`, a ~4 GiB `memcpy`. The `strlcpy` at `0x101cccf4`
then gets `1024 - (stored_len + len2)`, again computed after both copies.

**Overwrite map** (offsets from the copy destination `sp+16`, frame 2 160, LR saved at `sp+2164`):

| bytes written | clobbers |
|---|---|
| 2 100 | saved `r21` |
| 2 104 / 2 108 / 2 112 | `r22` / `r23` / `r24` |
| 2 116 / 2 120 / 2 124 | `r25` / `r26` / `r27` |
| 2 128 / 2 132 / 2 136 | `r28` / `r29` / `r30` |
| 2 140 | `r31` |
| 2 144 | caller frame start (back chain) |
| 2 148 | **saved LR** |

So the wire shape that reaches LR is a short host plus a first path segment of ~2 130+ bytes
(`x-file-cifs://h/<2130 a's>/x`), because a host long enough to push the span past 1 024 zeroes the stored
length and collapses the first copy.

**Live status: not yet confirmed.** `.202` and `.81` both refuse `SetAVTransportURI` with 1023 in the
current fleet state, and `AddURIToQueue` accepts the same DIDL (200 at host 50/1 200/3 000 and at path
2 100/2 200) without crashing — so the DIDL `<res>` alone does not reach this copy and the route is still
the open edge (see §1's caller note).

### 2.2 `0x1a0d40` — first append into a 1 024-byte inline buffer, check applied to the wrong copy

```
101a0d30  addi 22, 1, 16
101a0d34  lwz  5, 4232(27)      ; len <- obj+4232
101a0d38  mr   3, 22
101a0d3c  addi 4, 27, 4236      ; src = obj+4236 (inline)
101a0d40  bl   memcpy
101a0d5c  sub  0, 28, 22
101a0d60  subfic 0, 0, 1024
101a0d64  cmpw 7, 0, 29
101a0d68  bt   29, …            ; skip the NEXT chunk if it does not fit
```

Function `0x101a0b70`, frame 1 104. The `1024 - (dst+len)` arithmetic at `0x101a0d5c`–`0x101a0d68` is
computed **after** the copy and decides whether the *next* chunk fits, so the first copy is unbounded.
The struct carries a 1 024-byte inline buffer at `+4236` with its length at `+4232`.

### 2.3 `0x372b00` — log formatter, 4 096-byte buffer, only a non-zero test

```
10372ad0  li   5, 4096
10372adc  li   4, 0
10372ae0  bl   memset           ; memset(sp+16, 0, 4096)
10372ae4  lwz  5, 16(31)        ; len <- obj+16
10372aec  cmpwi 7, 5, 0         ; the only test
10372af8  mr   3, 28            ; dst = sp+16
10372afc  addi 4, 31, 28        ; src = obj+28
10372b00  bl   memcpy
```

Function `0x10372ab8`, frame 4 144. Saved `r27`–`r31` at `sp+4124`, LR at `sp+4148`: over 4 108 bytes
clobbers the frame. Sixteen call sites (`ac_impl`, `chsrc`, `dp_zpimpl`, the zone-group builders, the
crash uploader) — it is the shared "tag + struct" logger, so the length is whatever the logged struct
holds, not a request field directly.

### 2.4 `0x3747a4` — heap destination, reached straight from `/spotifyzc`

```
10374798  addi 3, 29, 32        ; dst = heap + 32
1037479c  lwz  5, 4(27)         ; len <- 27+4
103747a0  lwz  4, 12(27)        ; src <- 27+12
103747a4  bl   memcpy
```

Function `0x103745d8`, called directly by the `/spotifyzc` handler `0x1008f208` — the only
handler-reachable site a depth-1 call-graph walk from all 18 pre-auth handlers found. No length check in
the block; the destination is a heap object, so the failure mode depends on the allocation size.

### 2.5 `0x22abac` — OGG page body into an object field

```
1022ab80  stw  29, 6008(31)     ; len <- parsed page size
1022aba0  lwz  5, 6008(31)
1022aba4  mr   4, 28
1022aba8  addi 3, 31, 8         ; dst = obj+8
1022abac  bl   memcpy
```

Function `0x1022a62c`. The function's own strings include `ogg page size %d is larger than buffer %d`, so
a check exists somewhere in the page path, but `0x1022abac` itself has none and `6008` is written
straight from the parsed value at `0x1022ab80`.

## 3. The 57.23 route table (new)

`0x1052d000`, 28-byte entries `{name, handler, flags, …}`. This is the first complete list; the docs
carried six names. Pre-auth handlers, in table order:

| route | handler | route | handler |
|---|---|---|---|
| `/notify` | `0x1028a5b8` | `/advconfig` | `0x1028a724` |
| `/xml/device_description.xml` | `0x1028b1a4` | `/testpoint` | `0x1000a858` |
| `/xml/group_description.xml` | `0x1000cef4` | `/diaglevel` | `0x1028acc8` |
| `/getaa` | `0x10009e64` | `/diagmsg` | `0x1028ad14` |
| `/getrs` | `0x1028a5a8` | `/jobs` | `0x1036c570` |
| `/getsetting` | `0x1028a5b0` | `/set-cr-netstart-dest-mac` | `0x1000c870` |
| `/indexrepl` | `0x1000a2a0` | `/reset` | `0x1028ad74` |
| `/msprox` | `0x1000a4c0` | `/sonarctl` | `0x1000c980` |
| `/unlock`, `/unlock.htm` | `0x102a8e14` | `/wifictrl` | `0x1000cb78` |
| `/devmode` | `0x1028acf4` | `/cloudqueuepoll` | `0x1000ccf8` |
| `/testenv` | `0x1028c258` | `/info` | `0x1000c604` |
| `/customsd`, `/customsd.htm` | `0x101516ec` | `/websocket/api` | `0x100649ec` |
| `/tools`, `/tools.htm` | `0x10066eac` | `/spotifyzc` | `0x1008f208` |
| `/ping` | `0x10066e94` | `/spotdbg` | `0x1000d10c` |
| `/mdnsannounce` | `0x1000cf8c` | `/save_eq_presets` | `0x1000a5e4` |
| `/udplog` | `0x101a5808` | `/getDSP` | `0x1000a8c4` |
| `/putDSP` | `0x1000aa0c` | `/audio_tap` | `0x1000abd8` |

`/notify`, `/getaa`, `/getrs`, `/getsetting`, `/unlock`, `/devmode`, `/udplog`, `/info`, `/tools`,
`/customsd`, `/websocket/api`, `/spotifyzc` and the DSP pages answer with no devmode bit on either line.
`/advconfig`, `/diaglevel`, `/diagmsg` and `/testenv` are open on 57.23 and 403 on 86.10 (bit 18);
`/reset` is bit 22 on both. The bit map is in `FIRMWARE_PATHS.md`.

## 4. 86.10 sites, checked

### 4.1 `0x5f59b4` — `sonos::json::Stream` string read: BOUNDED

Clamp immediately before the copy: `0x5f59a2` `addiu v1,-1`, `0x5f59a4` `subu a3,v1,a1`, `0x5f59a6`
`sltu s1`, `0x5f59aa` `move a3,s1` — the length is `min(capacity-1-offset, available)`. The copy at
`0x5f59b4` uses `a3`.

### 4.2 `0x4f80c4` — `RKVReportData::addPair`: BOUNDED

`0x4f80a2` `lw v0,0(s0)` (cap), `0x4f80a4` `subu a0,v0`, `0x4f80a6` `sltu a0,v1` (needed = key+1+value+1),
`0x4f80aa` `btnez` → skip when it does not fit. Both copies (`0x4f80b8`, `0x4f80c4`) run only inside the
fit.

### 4.3 `0x4cd3ea` — JSON string append: BOUNDED

`0x4cd3ac` `addu v0,s1,v0`, `0x4cd3ae` `sltu a0,v0`, `0x4cd3b0` `btnez` → grow path (`0x4cd3d2`), else
the copy at `0x4cd3ea` with `a2 = s1` fits.

### 4.4 `0x4aa6b2` / `0x4aa6ec` — `/notify` body and params: BOUNDED

Body read `4f5990(req, sp+32, 4096, 0, 2000)` at `0x4aa6b2`; the query parser then gets 32/128-byte
destinations at `0x4aa6ec` (`a2=32`, `a1=sp+4256`) and `0x4aa6de` (`a2=32`, `a3=sp+4128`).

Also checked and bounded: devmode statement `memcpy` `0x4f3bc6` (length `<1025` enforced at `0x4f3b56`),
the strdup-style sites `0x49b802` and `0x65a244`/`0x65a276` (strlen+1 then malloc), and the ring copy
`0x5a8d06` (`sltu` at `0x5a8cf6`/`0x5a8cfe`).

## 5. 57.23 sites, checked and bounded — do not re-audit

| site | guard |
|---|---|
| `0x10293e34` | `cmplwi 7, 5, 15` at `0x10293e28`, 16-byte buffer |
| `0x10293f70` | `cmplwi 7, 5, 10` at `0x10293f58`, 16-byte buffer |
| `0x10131448` | `cmplwi 7, 5, 256` at `0x10131438` |
| `0x1031fba8` | URI host, but the URI is `strlcpy`'d into a 192-byte field at `obj+7164` first (`0x1031fb58`), so the host is ≤ 191 into a 1 040-byte buffer |
| `0x10324224`, `0x10126b88` | string-builder helpers; capacity/length pairs at `+3100/+3104` and `+20/+24` with doubling loops |
| `0x1000c934` | `/set-cr-netstart-dest-mac` `sonosDecodePercentEncodedStr(src+4, sp+16, 1025, &n)` (`0x10546ba8`) — explicit 1 025 max into a 1 040-byte buffer |
| `0x101a5934` | `/udplog` tokenizer with explicit 32 and 1 025 limits |
| `0x10244388` | mbedTLS record copy; the length is the caller's buffer size |

## 6. Status

Two unbounded packet-length copies, both in the same URL/path cluster: `0x1007b2c4` (the headline, a
>5.3 KB URL) and `0x101ccc9c` (a >2.1 KB "long value"). The rest of the sampled surface is careful; the
parsers that matter most (JSON, SOAP args, GENA body) clamp.

Next, in order: (a) identify the vtable callers of the `0x101a0b70` / `0x101cc878` / `0x101ddd2c` cluster
so the reachable request is named, (b) craft a >5.3 KB URL and a >2.1 KB value on a muted player with
`/status/zp` before and after plus ICMP. Neither has been run yet.

## Live: the 57.23 URL-span overflow fires

`/getaa?u=x-file-cifs://<host>/SHARE/<N×'A'>.jpg` on the S1 Play:5 (`.202`, 57.23), `N` swept:

| N | answer | player after |
|---|---|---|
| 1000, 3000 | 404 (normal, no such file) | up |
| 5300, 5400, 5800, 6000 | 408 or 000 | up again within seconds |
| 5600 | **000** (connection dies) | up |

So the fault zone opens at roughly the predicted frame size (the frame is 5 344 with the destination at
`+16`, so ~5 328 bytes of span is the limit) and the outcome is hang-or-crash rather than a clean return.
This is a **stack write** primitive reached pre-auth from a single GET, which is the thing the SMB
over-read lacks. Next: find the exact N where the saved LR is overwritten (the 408/000 split suggests
the return address is already being clobbered before the frame is fully overrun), and whether the
overwritten bytes are attacker-shaped enough to steer control.

### 1a. Exact overwrite map (finding 1, `0x1007ad60`)

Frame `stwu 1,-5344(1)`; destination `r1+16` (`0x1007b2b4`); length = the span at `r1+5244`
(`0x1007b2b8`); source = the **original URL base** `r18` (`0x1007b2bc`); `memcpy` at
`0x1007b2c4`; then `stb 30,16(9)` with `9 = r1+span` writes a NUL one byte past the copy
(`0x1007b2d8`). The epilogue restores from these same slots and does `mtlr 0` at `0x1007b084`.

Saved-slot offsets **from dst** (`dst = r1+16`), all restored in the epilogue:

| dst off | slot | dst off | slot |
|---|---|---|---|
| 5252 | CR (`mtcrf` at 0x1007b08c) | 5296 | r24 |
| 5256 | r14 | 5300 | r25 |
| 5260 | r15 | 5304 | r26 |
| 5264 | r16 | 5308 | r27 |
| 5268 | r17 | 5312 | r28 |
| 5272 | r18 | 5316 | r29 |
| 5276 | r19 | 5320 | r30 |
| 5280 | r20 | 5324 | r31 |
| **5284** | **r21** | **5332** | **LR** (4 bytes, 5332-5335) |
| 5288 | r22 | 5328 | caller frame begins (r1+5344) |
| 5292 | r23 | | |

So the thresholds are: `N ≤ 5252` safe; `N ≥ 5253` clobbers CR; `N ≥ 5288` reaches r21;
`N ≥ 5328` reaches r31; `N ≥ 5336` overwrites all four LR bytes; `N ≥ 5337` writes into the
caller's frame. The NUL lands at `dst+N`.

**What the attacker controls:** the URL bytes themselves, raw (the source is the original
string, not the percent-decoded copy), so every clobbered byte is attacker-chosen. The one
structural constraint is that the span ends at the **second `/`** after `url+14` (`strchr` at
`0x1007ae34`/`0x1007ae48`, span `= second_slash - url + 1` at `0x1007ae60`), so the last copied
byte is always `/` (0x2f). For a clean LR overwrite use `N = 5337`: LR bytes 5332-5335 are free
and the `/` lands at 5336 in the caller's frame.

### 1.1 Reachability, measured live on .202 (2026-10-26)

The splitter runs on the **request target before routing**, so every route
reaches it and no file has to exist:

| request | n=10 | n=5300 | n=5400 |
|---|---|---|---|
| `/zzz/<A*n>` (no such route, no file) | 404 | 408 | 000 |
| `/info/<A*n>` (fixed route) | 404 | 408 | 408 |
| `/info?x=<A*n>` (long query, short path) | 200 | 408 | 408 |
| absolute-form target `http://host/zzz/<A*n>` | — | — | 408 |
| `/devmode`, `/art/`, `/raw/`, `/msprox?uuid=` | 404/405 | 408 | 000/408 |

- The flip is at the frame, not the route: 404 up to n=10, 408 from n≈5300,
  and connection resets (000) at n=5330-5336, i.e. just past the 5328 bytes
  between `dst` (`+16`) and the end of the 5344-byte frame. n=5340 is flaky
  (408 or 000) because the smash lands in the caller's frame.
- 408 is the daemon's own answer, not a restart: the body is the stock
  `Error 408` page and `/info` is byte-identical before and after. So 408 is
  the splitter's long-span path and 000 is the smash.
- Both the path and the query count, and an absolute-form target works, so the
  span is the whole request target, not a path component.
- **S2 is unaffected**: `.86` and `.81` (86.10) answer 404 for the same
  5400-byte path. This is a 57.23-only bug.
- The three callers stay vtable-dispatched: `0x101a07d0`, `0x101a0b70` and
  `0x101ddd2c` have no direct `bl` to them anywhere in the image.

### Corrected live picture (the 408 is the player's own answer, not a timeout)

Timed with `-m 25`: `N=4000` answers **408 in 0.02 s** — instant, so it is the daemon rejecting, not a
slow fetch. `N=5600` answers 000 in 0.03 s. Repeating `N=6000` three times gives 000, 408, 408, and
`/status/zp` stays 200 throughout: the daemon does **not** die.

So the shape is: ≤2500 processed (404), ≥4000 rejected with an instant 408, occasionally the connection
dies instead. The copy is reached (the frame is 5344 and the crash zone starts above it) but the
overwrite lands in the caller's frame without taking the daemon down — consistent with the audit's own
note that the destination is `sp+16` and the *caller's* frame follows. To make it a control-flow bug we
need either a route without the 408 pre-check, or enough length to reach the saved LR of the outer
frame.

## Who reaches the splitter, and whether a file must exist (2026-10-26)

The splitter is `0x101a02ac`. It is reached only through the `x-file-cifs:`
URL handler `0x1007ad60` (`strncasecmp` against `x-file-cifs:` at
`0x1044ede8`, 12 bytes), and `0x1007ad60` has exactly **three** callers:

| caller | enclosing function | what it is |
|---|---|---|
| `0x101a06b4` | `0x101a05b4` | CIFS share/path splitter — `lookup resulted in path w/o share` |
| `0x101a0c9c` | `0x101a0b70` | `atomtable` helper |
| `0x101cc704` | `0x101cc6c0` | `trackValidateURI` — `unplayable queue entry`; called from `0x101cc878` (queue context) and `0x101ddd2c` (saved-queue DIDL) |

**No file has to exist, and no SMB session is opened first.** In `0x101a05b4`
the call at `0x101a06b4` is followed only by string helpers (`0x10415590`,
`0x1007b36c`) — no connect, no open. The overflow is a pure URL-parse event,
so the URL only has to be well-formed enough to reach the splitter.

Routes that deliver one:

- `GET /getaa?u=x-file-cifs://…` — live-confirmed by the owner on `.202`:
  N=5300–6000 answer 408 or 000. The handler `0x10009e64` is a thin dispatcher
  into `0x100fe4dc`, which reaches the CIFS code through the AVT/URI layer
  rather than directly, so the reach is indirect but real.
- `AddURIToQueue` / `SetAVTransportURI` with an `x-file-cifs:` URI — the URI
  matrix in `RE_URI_HANDLING.md` shows Add accepting that scheme at 200/30 B
  and rejecting at 804/4 KB, which is the length gate beside this splitter.

### What the overwrite actually is (verified at the instruction level)

```
1007b2b4  addi 31, 1, 16      ; dst = sp+16
1007b2b8  lwz  5, 5244(1)     ; len = span (no clamp anywhere)
1007b2bc  mr   4, 18          ; src = ORIGINAL url base (not the decoded copy)
1007b2c0  mr   3, 31
1007b2c4  bl   memcpy
1007b2d8  stb  30, 16(9)      ; NUL at dst+span  <- second, one-byte primitive
```

Two consequences that decide exploitability:

1. **The bytes are literal URL text.** The source is the raw URL, so the overwrite is ASCII — a saved LR
   becomes `0x41414141` for `AAAA`, which is not a mapped address. Percent-encoding does not help here
   because the copy does not run on the decoded buffer.
2. **The NUL write at `dst+span` is independent of the copy** and lands one byte past it. Choosing the
   span so that `dst+span` sits on a saved pointer gives a clean single-NUL corruption, which is the
   more promising of the two.

The frame is 5 344 with the destination at `+16`, so the copy leaves the frame at a span of 5 328 and
the NUL follows it.

**What the NUL can reach (offsets from `dst = sp+16`).** The prologue saves `CR` at `sp+5268`, `r14`–`r31`
at `sp+5272…5340` and LR at `sp+5348`, i.e. at these distances from the destination:

| span | slot | note |
|---|---|---|
| 2 100 | first byte past the 2 100-byte buffer | unused stack |
| 5 224 / 5 228 / 5 240 | locals `sp+5240`, `sp+5244`, `sp+5256` | `sp+5244` **is the offset the NUL itself uses** — self-modifying |
| 5 252 | saved `CR` | condition-register word |
| 5 256 … 5 324 | saved `r14` … `r31`, step 4 | |
| 5 296 | saved **`r24`** | the URL base pointer, reused as a copy source at `0x1007b328` |
| 5 312 | saved **`r28`** | the object pointer: `lwz 9,0(28)` → `lwz 9,8(9)` → `bctrl` at `0x1007b2f4` |
| 5 332 | saved **LR** | return address |
| 5 344 … 5 400 | caller frame `sp+0 … sp+56` | back chain, then the caller's own locals |

The three worth naming, best first:

1. **`r28` at span 5 312.** Zeroing its low byte moves the object pointer down by `obj & 0xff`; the very
   next use is an indirect call through `*(*(r28)+8)`, so the vtable load comes from a shifted address —
   a controlled indirect call rather than a crash.
2. **LR at span 5 332.** Zeroing the low byte of the return address re-enters the caller's 256-byte page
   at `LR & ~0xff`. For caller `0x101cc704` that lands on `0x101cc700` (`addi 6,1,12` falling straight
   into `bl 0x1007ad60`), i.e. the splitter re-entered with the caller's registers still live.
3. **`r24` at span 5 296.** The URL base, later used as a `memcpy` source; zeroing its low byte shifts
   that source by `url & 0xff`.

Every one of these is a *single* zeroed byte with the rest of the pointer intact, which is why the NUL is
the usable primitive and the ASCII copy is not. The live runs only degrade above ~4 000, so something
between the splitter and the copy still consumes the first ~2 000 bytes of span; that gap is the next
thing to resolve.

## 2026-10-26: which routes reach the splitter (live on .202)

**Any path reaches it, and no file has to exist.** A bare `/<A*N>` behaves
identically to `/status/<A*N>` — the splitter runs in the generic URL dispatch
before route matching, so the 404 at small N is the router's answer *after* the
copy has already happened.

Live, `.202`, HTTP only:

| N | bare `/<A*N>` | `/status/<A*N>` |
|---|---|---|
| 1000 | 404 | 200 |
| 3000 | 404 | 200 |
| 4000 | 404 | 200 |
| 4010 | 404 | — |
| 4020 | 408 | 408 |
| 4500 | 408 | 408 |
| 5300 | 000/408 | 408 |
| 6000 | 408 | 408 |

The break is between 4010 and 4020, not the ~5328 the frame math predicts, so the
destination offset or the span base needs revisiting. The daemon recovers at
once (`/status` and `/xml/device_description.xml` both 200 immediately after), so
the smash costs the connection, not the process.

Static corroboration: the splitter `0x1007ad60` has exactly three call sites —
`0x101a06b4` (in `0x101a05b4`), `0x101a0c9c` (in `0x101a0b70`), `0x101cc704` (in
`0x101cc6c0`). None of the 26 route handlers reaches it by a direct `bl` chain
(BFS to depth 8 from `/getaa`, `/getrs`, `/getsetting`, `/status`, `/raw`
explores 0/152/152/176/0 functions and never hits it), and the three entry
functions have no `bl` caller and no pointer anywhere in the image — they are
`jalr` targets, which is exactly why the reach is route-independent.

Before the copy the splitter calls only `strncasecmp`, `strchr`, `strrchr`,
`strlen`, `strcpy`, `memcpy`, `isalpha` and `sonosDecodePercentEncodedStr` — no
`open`/`stat`/`fopen` — so nothing about the target file can gate the write.

### The 408 is a length gate, and it may be pre-empting the copy

A long *share* (`x-file-cifs://host/<5000 A>/x.jpg`, span ≈ 5 013 by the splitter's own arithmetic) also
answers 408, and the daemon stays up. So the 408 tracks total URL length, not the span, and it arrives
before the copy can matter.

That makes the next question concrete: **is the 408 emitted before or after the copy?** If before, this
route never reaches the overflow and the primitive needs a different entry (the same splitter is called
from `0x101a06b4`, `0x101a0c9c`, `0x101cc704`, and a SOAP or POST body has no URL-length gate). If
after, the copy runs and the 408 is just the response the crashed frame never sends.

### 2.1a Overwrite map (frame 2160, dst sp+16, buffer 2100)

`stwu sp,-2160(sp)` at `0x101cc878`; saves `r21`@2116, `r22`@2120, `r23`@2124, `r24`@2128,
`r25`@2132, `r26`@2136, `r27`@2140, `r28`@2144, `r29`@2148, `r30`@2152, `r31`@2156, LR@2164
(`0x101cc8b0`–`0x101cc8c8`), restored in the same order at `0x101cc8ec`–`0x101cc91c`. The copy is
`memcpy(sp+16, obj+4212, obj+4208)` at `0x101ccc9c` with `r3 = r23 = sp+16` (`0x101ccc64`).

| N (bytes copied) | first byte touched | fully overwritten |
|---|---|---|
| 2101 / 2104 | r21 | r21 |
| 2105 / 2108 | r22 | r22 |
| 2109 / 2112 | r23 | r23 |
| 2113 / 2116 | r24 | r24 |
| 2117 / 2120 | r25 | r25 |
| 2121 / 2124 | r26 | r26 |
| 2125 / 2128 | r27 | r27 |
| 2129 / 2132 | r28 | r28 |
| 2133 / 2136 | r29 | r29 |
| 2137 / 2140 | r30 | r30 |
| 2141 / 2144 | r31 | r31 |
| 2145 / 2148 | caller back chain | back chain |
| 2149 / 2152 | **LR** | **LR** |

`r30` is the object pointer and `r31` the source string, so both are live at the epilogue.

### 2.1b The setter does not store a long length — it stores only at exactly 1024

`0x101cbbc8` is not the `>1024` arm. Decoding the two branches:

- `0x101cbbbc` `cmplwi 7,3,1024`; `0x101cbbc0` `41 82 00 0c` — BO=12 (bit0=0), BI=2 (CR0 EQ):
  **branch if `r3 != 1024`** → `0x101cbbcc` (return, no store).
- `0x101cbbc4` `40 9d 00 24` — BO=4, BI=29 (CR7 GT): branch if `r3 <= 1024` → `0x101cbbe8`
  (`stw 3,4208`). The `>1024` arm at `0x101cbbc8` (`stw 29,4208`, `r29` = `obj+4` from the
  constructor at `0x1007a928`) is therefore only reached for `r3 > 1024` **if** the first branch is
  taken on equality — which the BO/BI decode says it is not.

Live check on `.84` (the coordinator; `.202` is a stereo slave and answers 1023): `SetAVTransportURI`
with 2100, 2150, 2200 and 3000-character URIs all return 200, `/status/zp` 200 after each, and
`GetPositionInfo` returns the 3000-character URI intact. Nothing in 2100–3000 crashes, which matches
"length stays 0" rather than "length is the URI length".

No branch or `bl` in the whole function targets `0x101cbbc8` (scanned every `bc`/`bl` in
`0x101cbb38`–`0x101cbc08`), so the `>1024` arm is unreachable. The length is stored **only when it is
exactly 1024**, and 1024 never reaches the saved registers.

So 2.1 is a real unchecked copy but not a live overflow on this build: the field it copies is either
0 or 1024. Making it a >2100 overflow needs a setter path that stores the length unconditionally, or
the `>1024` arm to be reachable through a different entry.

### Correction: on the `/getaa` form the span is only the share, not the path

The span is computed once (`stw 9, 5244(1)` at `0x1007ae70`) and never updated, and it is
`second_slash - url_base` where the search starts at `url+14` (past `x-file-cifs://`). For
`x-file-cifs://host/SHARE/file.jpg` the second slash is the one *after the share*, so the span is about
18 bytes — the long filename never enters it. A bare `/<A*N>` path has no slash after `+14` at all, so it
takes the NULL branch and may not reach the copy either.

So finding 1 is real code but the `/getaa` shape does not drive it with a long span. The live 4010→408
break on long paths is therefore **something else**, and the promising target is finding 2.1
(`0x101ccc9c`, a 2 100-byte stack buffer whose setter only stores values *longer than 1 024* and imposes
no upper bound) — that one is fed by a "long value" field, not by a URL split.

### Reconciled: the bare-path branch is the one that overflows

`recon/urlspan_source.py` settles the source question: the same raw length breaks with `A` and with
`%41`, and `%41` is still broken at 36 000 raw characters, so the copy reads the **raw target** and the
decoded buffer is not involved. Combined with the earlier correction: the `x-file-cifs://` form only
puts the *share* in the span, but a **bare path** takes the other branch, where the span is the whole
target — and that is the branch that breaks at ~4 020.

So the 408 is not a length gate in front of the copy; it is the splitter's own failure answer *after*
the copy has already run. The daemon survives because the overwrite lands in the caller's frame rather
than on the return address, which is the next thing to fix: the span must be pushed past 5 328 and the
bytes at the saved-register offsets must be chosen deliberately.

### What the ASCII-only source means for control flow

The copy is raw target text, so every overwritten byte is printable ASCII. A saved LR becomes
`0x41414141` for `AAAA`, which is not a mapped address — so a straight return-address overwrite does not
land anywhere useful, and the daemon survives because the overwrite stops in the caller's frame.

Two ways forward, in order of promise:

1. **The NUL write is the real primitive.** `stb 30, 16(9)` writes one `0x00` at `dst+span`, exactly one
   byte past the copy, and the span is chosen by the request. Placing that NUL on the low byte of a saved
   pointer shifts it down by up to 255 bytes — a classic partial-pointer overwrite that needs no
   non-printable bytes at all.
2. **Reach a frame whose LR is at an offset the span can hit**, and accept the `0x41414141` return only
   as a crash oracle, not as control.

Anything that needs a real address in the payload has to come from somewhere other than the URL text
(a SOAP body or a POST body can carry raw bytes; the URL cannot).

### SOAP bodies carry long URLs past the request-line limit

`recon/gate_probe.py` on `.202`: a URL in the request line dies at ~4 010, but the same URL inside a SOAP
body does not — `SetZoneAttributes` with a 6 000-byte `DesiredIcon` answers **200** at every length
tested (3 900…6 000), and `SetAVTransportURI` answers 500 (bad URI, but parsed). So the request-line
limit is a transport limit, not the splitter's, and a SOAP body is the way to deliver a >5 328-byte URL
to whatever parses it.

Neither body form crashed the daemon at 6 000, so the splitter is either not reached from those two
actions or its span is not the whole icon string. That is the next thing to establish: whether
`SetZoneAttributes`/`SetAVTransportURI` route through `0x1007ad60` at all.

### Correction: the URL-span overflow does not kill the daemon

`recon/crash_oracle.py` polls `/status/zp` at 100 ms and classifies every probe. Result: **112
consecutive probes stayed 200** while `N=5800` answered 408, and the daemon was healthy afterwards
(`getaa` 404 again). So a 408/000 on the request is a *dropped request*, not a daemon death — my earlier
"crash" reading of 000 was wrong, and the SMB over-read (which really does drop `/status/zp` for 5–20 s)
is still the only confirmed daemon-level crash.

That means the URL-span copy is either not overflowing at all, or overflowing into a frame whose
corruption the daemon absorbs. Both are testable, and the oracle is now the instrument that decides it.

### 2.1c The NUL at `dst+span+1` and what a 2100–5400 span reaches

The terminator is `stb 0,17(9)` at `0x101cca68` with `r9 = sp + span` (`0x101cca60`), i.e. a single
zero byte at `dst + span + 1` = `sp + span + 17`. `span` is the distance between the first and last
`/` in the URL (`0x101cca44`), so one long path segment sets it directly.

The splitter's frame is 2160 and its caller is `0x101a016c` (frame 1072, called from `0x101cbe80`,
frame 9568). Offsets below are `span`; the address is `caller_sp + (span - 2143)`.

| span | target | note |
|---|---|---|
| 2099–2123 | splitter's saved r21–r27 | caller scratch |
| 2127 | splitter's saved **r28** | caller's `r28` = the splitter's object argument |
| 2131 | splitter's saved **r29** | caller's `r29` = `sp+16`, the URL buffer |
| 2135 | splitter's saved **r30** | caller's `r30` = the outer object |
| 2139 | splitter's saved **r31** | caller's `r31` = the iterator/vtable object |
| 2143 | caller back chain | |
| 2147 | splitter's saved LR | `0x101a0210` → `0x101a0200`, re-enters the call setup (benign) |
| 2151 | caller's flag byte at `sp+8` | |
| 3199–3211 | caller's saved r28–r31 | `0x101cbe80`'s registers, restored when `0x101a016c` returns |
| 3219 | caller's saved LR | `0x101ce414` → `0x101ce400`, both `b 0x101ce2f8` (benign) |

**Useful one: span 2139.** It zeroes the low byte of the caller's `r31`, and `0x101a0224` immediately
does `lwz 9,0(31)` / `lwz 9,12(9)` / `bctrl` — a vtable dispatch through a pointer shifted back to its
256-byte page base. `r28` at 2127 is the same trick on the object handed to the splitter, but it is not
dereferenced before the next call.

Both LR hits are benign because the zeroed targets land on equivalent instructions; the pointers are
the primitive, not the return address.

### 1b. SetZoneAttributes does **not** reach the splitter

`SetZoneAttributes` is a DeviceProperties action: the response builder is `FUN_10320bf4`
(`0x10320bf4`, action-table entry `0x104eb8f0`) and the request handler that calls it is
`FUN_1015c830` (`0x1015cf9c`). `DesiredIcon` is only a *name* there, and the handler's only
outbound call of interest is `FUN_10070300(…,"SpeakerOnly",…)` at `0x1015c8xx` — **no call to
`0x1007ad60` and none to its three callers** (`0x101a05b4`, `0x101a0b6c`, `0x101cc6c0`).

The splitter's real neighbourhood, one level up from those callers:

```
0x1007ad60 <- 0x101a06b4 (FUN_101a05b4) <- 0x101a0a38 (FUN_101a07cc) <- 0x1007f840
0x1007ad60 <- 0x101a0c9c (FUN_101a0b6c) <- 0x101a1f04/0x101a2164 (FUN_101a1c84) <- data 0x1046f8ac
0x1007ad60 <- 0x101cc704 (FUN_101cc6c0) <- 0x101ccc04/0x101ccc78 (FUN_101cc878), 0x101dddf8 (FUN_101ddd28) <- data 0x10477ddc
```

`0x1046f8ac` sits in a table surrounded by `Audiobooks`, `Movies`, `Playlist ID`, `Playlist
Items` — the **ContentDirectory** browse area. `0x10477ddc` sits next to `SavedQueues`.

So the SOAP `SetZoneAttributes`/`DesiredIcon` route is dead for this overflow; the reachable
route is ContentDirectory browse (or the SavedQueues handler), and the `/getaa?u=` shape used
in the live sweep is a different entry again.

**Bare paths do not reach 2.1c.** `/` + 2138/2139/2140 `A` + `/x` answers 404 with the daemon up on
`.202`, so the 2.1c NUL needs the ContentDirectory/SavedQueues route the lane identified, not a bare
path. The bare-path 404s that break at ~4 020 are a third thing again.

**A long `ObjectID` alone does not fire it.** `Browse` on ContentDirectory with a 2 139-byte ObjectID
answers a SOAP 500 with the daemon up, so the long value is not the object id — it is a value *inside*
the browse result (a title or a SavedQueues entry), which means the span has to come from data the
player already holds, not from the request. That is a materially different requirement from the URL
cases and is the thing to confirm next.

### 2.1c which field feeds the copy, and how it is set

The copy source is `obj+4212` with length `obj+4208`, and both are written by the item
constructor `FUN_101cbb38` (`0x101cbb38`): `param_1[0x41d] = param_2` (the URI string) and,
only for a CIFS URI, `param_1[0x41c] = first_slash_after_scheme + 1 - uri`.

So the copied value is **the URI prefix through the first `/` after `x-file-cifs://`** — the
host/share part — not a title and not the ObjectID. The store is conditional:

```
101cbbb4  sub  3, 3, 31        ; first_slash - uri
101cbbb8  addi 3, 3, 1
101cbbbc  cmplwi 7, 3, 1024
101cbbc0  bt 2, 101cbbcc       ; == 1024 -> skip
101cbbc4  bf 29, 101cbbe8      ; <= 1024 -> skip
101cbbc8  stw 29, 4208(30)     ; only > 1024 stores
```

So the field is non-zero **only** when the prefix already exceeds 1024, which is why the
2 100-byte buffer is reachable at all.

Wire settability: the URI is carried by the item, and items are created from browse results.
The wire entry that lets a chosen `res` URI into a queue is `AddURIToQueue` with DIDL metadata,
so a `x-file-cifs://<long host>/x` res can be planted and then read back by browsing the queue.

**Cheapest carrier on `.202`, and the 1 024 cap (2026-10-26).** `.202` is a slave of an unreachable
coordinator (`RINCON_000E585C3DF801400` at `192.168.5.137`), so `AddURIToQueue` answers 800 and
`BecomeCoordinatorOfStandaloneGroup` answers 401. The queue is therefore unusable there. The cheap
carrier that does work is **`CreateObject` into `FV:2`**: one SOAP call, no queue, no coordinator, no
audio — `200` and a fresh `ObjectID` (`FV:2/4`, `FV:2/5`).

But **every field of a browse result is truncated to 1 024 characters.** A 2 200-char `dc:title` comes
back as 1 023; a 2 200-char `res` comes back as 1 024 — and the same cap applies to a queue item
browsed on `.81` (res 1 024 of 2 200). Since the setter only stores a value when the prefix exceeds
1 024, and the prefix is `first_slash_after_scheme + 1`, a 1 024-char `res` gives a prefix of 1 025 →
the field is stored as 0, and a 1 022-char `res` gives 1 023. Either way the first copy is <= 1 023,
so **the browse result alone cannot overflow the 2 100-byte buffer**; the second copy (the first path
segment, also capped at ~1 009 by the same truncation) is the only candidate and it is still short.

So the open question is whether the item constructor runs on the *untruncated* string at
`AddURIToQueue`/`CreateObject` time rather than on the browse result — the truncation is in the
response, and a 2 200-byte `EnqueuedURI` is accepted by the queue on `.81` without a crash.

**There is no third field, but there is a second path (2026-10-26).** `FUN_101cc878` reads exactly two
object fields, `+0x1074` (source string) and `+0x1070` (offset/length), plus its `param_2` argument.
The argument only selects the branch:

```
101cc8xx  bl FUN_100a7138      ; 1 = known scheme (incl. x-file-cifs), 0 = unknown
101cc8xx  bl FUN_1007acb8      ; 1 = starts with "//" -> skip everything
101cc8xx  bl FUN_1007acf0      ; 1 = starts with "/" or "X:/" -> path A, 0 -> path B
```

Path A is the bounded `memcpy(dst, +0x1074, +0x1070)` already described. **Path B treats `+0x1070` as
a start offset and copies to the *last* `/` in the stored string**:

```
101cc9xx  lwz  4, 4212(30)     ; str  = obj+0x1074
101cc9xx  lwz  3, 4208(30)     ; off  = obj+0x1070
101cc9xx  bl   strrchr(str,'/')
101cc9xx  bl   strchr(str+off,'/')
101cc9xx  sub  5, last, (str+off)
101cc9xx  bl   memcpy          ; dst = sp+16, len = last_slash + 1 - (str + off)
```

So the length is `last_slash + 1 - (str + off)` — driven by the *stored string*, not by the field, and
unbounded. A stored URI with many `/` after the offset gives a long copy even with a tiny `+0x1070`.
Both branches need `+0x1070 != 0`, which only the x-file-cifs setter produces, so the object has to be
built with a CIFS URI and then called with a non-scheme argument to take path B.

The setter exists twice with identical logic — `FUN_101cbb38` (vtable `0x10475a48`) and `FUN_101cc518`
— so both object flavours behave the same; there is no setter that stores anything other than the
prefix or 0.

## 7. What actually feeds 2.1: a CIFS failure-message formatter, not a title

Correction to the framing in §2.1. `0x101cc878` is not a generic "long value" reader — it is the
**formatter for `x-file-cifs:` failure messages**, and the long value is a *message detail string*.

Evidence, all inside `0x101cc878`:

```
101cc958  lis 11, 4165 / addi 9, 11, -4632   ; 0x1045EDD8 = "x-file-cifs:"
101cc96c  lwz 0, 4(9) / lwz 11, 8(9) / lbz 8, 12(9)
101cc97c  stw 10, 16(1) / stw 0, 20(1) / stw 11, 24(1) / stb 8, 28(1)   ; 13-byte literal into sp+16
101cc98c  bl 0x103897b8                       ; bounded copy of the URL into sp+28, limit 1013
101cc9f8  lwz 28, 4208(30)                    ; the stored length
101cca0c  lwz 29, 4212(30)                    ; the stored string
101cca18  bl 0x10546b00                       ; strrchr(str, '/')
101ccc9c  bl memcpy                           ; sp+16 <- *(obj+4212), *(obj+4208)
```

The store side is `0x101cbb38(obj, str, 0, 0)`, which puts `str` at `obj+4212`, zeroes `obj+4208`, and
only records a length at `obj+4208` when it exceeds 1024 (`0x101cbbc4`/`0x101cbbc8`). It has **six call
sites** — `0x101cbd78`, `0x101cbec0`, `0x101cd168`, `0x101cd2b4`, `0x101cdb5c`, `0x101cdca0` — all in the
CIFS client, all passing a detail string.

Consequence for routing: the wire field is **whatever ends up in a CIFS failure message** — the URL, the
share, or the path — not a queue title or a DIDL title. `/getaa?u=x-file-cifs://…` is therefore the
natural driver, and the same request shape as finding 1. A long ObjectID on Browse producing a 500 is
consistent with this (the id lands in a message), but the id itself is not the copy source.

**The saved-queue route on S1 is `CreateObject`, not `CreateSavedQueue`.** The 57.23 ContentDirectory
SCPD has no `CreateSavedQueue` (401 Invalid Action); it exposes `CreateObject`/`DestroyObject`, and
`Browse` on `SQ:0` returns a DIDL container, so the container exists. `CreateObject` with a DIDL item
answers 713 (Object not found), i.e. the `ContainerID`/`parentID` pair is wrong, not the title. Once a
create succeeds with a short title, the same call with a 2 139-byte `dc:title` is the 2.1c trigger.

**The favourite title is capped at 1 023 bytes, which is below the 2.1c threshold.** `CreateObject` into
`FV:2` succeeds (200, object `FV:2/19`) with a 2 139-byte title, but the device stores 1 023 — and the
2.1c setter only stores values *longer than* 1 024. So a favourite title cannot reach 2.1c by itself;
the long value has to come from a field with a bigger cap (the `res`/art fields are capped at 37/64 in
the same test, so not those either).

### 2.1d CreateObject attempts on .202 (2026-10-26)

Path matters: `ContentDirectory/Control` under `/MediaRenderer` answers **405**; the service is
on `/MediaServer/ContentDirectory/Control`.

With a well-formed DIDL-Lite item (`dc:title`, `upnp:class`, `res` with `protocolInfo`) and
`ContainerID` `SQ:0`, CreateObject still answers **500 / UPnPError 713** ("object does not
exist"). `Browse` on `SQ:0` answers 200 with an **empty** `DIDL-Lite`, so the queue container
exists but holds nothing — the suggested "non-empty queue first" precondition is real and
unmet.

Making it non-empty is the current blocker: `AddURIToQueue` on `/MediaRenderer/Queue/Control`
answers **500 / 401** (invalid args) for every shape tried — bare URI, URI plus DIDL metadata,
`DesiredFirstTrackNumberEnqueued` 0 and 1, `EnqueueAsNext` 0 and 1, `x-file-cifs://` and
`http://` URIs. So the argument contract for AddURIToQueue is the next thing to read, not the
DIDL.

### The serializer's title+blob carrier: `SetAVTransportURI` with a DIDL

The 1112-byte serializer (`0x10219cfc`, API subsystem, record type `0x98`) reads its blob from
`obj+3672` with the length at `obj+4768` (`0x10219874` `addi r31,r28,3672`, `0x10219878`
`lwz 0,1096(r31)`). The buffer is 1100 bytes: `memset(obj+3672, 0, 1100)` at `0x1021a3f8` and a
template copy from `0x1044cc34` at `0x1021a41c`.

The only request that fills both fields is `SetAVTransportURI` with a DIDL carrying `dc:title` and
`r:resMD` — the title is the string, `r:resMD` is the blob. Verified live on `.84`: the call returns
200 and the player stores the track (the read-back `TrackMetaData` is rebuilt from the URI, so the
stored blob is not echoed).

The API route family that owns the record is in the descriptor table at `0x1044cc34` —
`upnp`, `upnpMusicServices`, `upnpQueue` — i.e. `POST /api/v1/players/local/upnp*?protocolVersion=1`.
Those routes answer 200 but return an empty `parameters` list even for `GetZoneGroupState`, so the
blob cannot be read back through the proxy as it stands.

### Corrections and a new lever

- `CreateSavedQueue` and `AddURIToQueue` live on **AVTransport**, not ContentDirectory — that is why
  ContentDirectory answered 401 for them.
- The 1 023-byte cap is on **characters**, so a multi-byte title beats it in bytes: `CreateObject` into
  `FV:2` with 1 023 × `Ж` (2 bytes each = **2 046 bytes**) succeeds (`FV:2/20`). Browsing `FV:2` with
  that title still did not fault (oracle: 286 samples, 0 downs), so the cap was not the only obstacle.
- `AddURIToQueue` on AVTransport answers 800 for `x-sonos-http:` (raw and percent-encoded) and
  `x-rincon-mp3radio://`, with and without `upnp:class` and `protocolInfo`, on an empty queue. The URI
  form is the thing to get right; the queue record is where the serializer's title+blob would come from.

### The queue blocker was the stereo pair, not the URI

`.202` is the **right** channel of a stereo pair (`ZoneName "X (R)"`), and every AVTransport queue
action answers 800 on it. The coordinator is `.84` (`ZoneName "X"`, `RINCON_000E5880108A01400`), and the
same `AddURIToQueue` there answers **200** with a track number. So all queue work must be addressed to
the coordinator — the same rule the controller uses for group commands.

With that, a track was queued carrying a 255-byte title and a 1 373-byte metadata blob (the setter caps
the blob at 0x400, so it lands on the exact 174-byte overrun case from the budget tool), then selected
and played (`Seek TRACK_NR 6` → 200, `GetPositionInfo` shows the CIFS URI). The daemon stayed up
throughout (oracle: 223 samples, 0 downs), so the serializer overrun is either not on the play path or
benign at these sizes.

**The serializer overrun is not observable on play or on any status page.** Playing the padded record
(CIFS mp3) and then the HLS track, and hitting all eight `/status/*` pages, left the daemon up every
time (oracle: 240 and 281 samples, 0 downs). With the blob capped at `0x400` and the title at 255 the
maximum overrun is ~174 bytes into `.data`, so it is very likely benign by construction — a weak
primitive, not a crash.

`CreateSavedQueue` on the coordinator (AVTransport, correct service now) answers **811** with a
2 046-byte title and the queue as the source, so the saved-queue route still needs its argument shape.

### The cap is 1 023 **bytes**, and it is one byte short

`CreateSavedQueue` on the coordinator works (200, `AssignedObjectID SQ:0`) once `EnqueuedURIMetaData`
carries real DIDL — that was the 811. But the stored title is **1 023 bytes** (512 × `Ж`), the same cap
as favourites, and the 2.1c setter's guard is `cmplwi 7,3,1024` (`0x101cbbbc`) — an unsigned "store only
if greater". So every title path lands exactly **one byte short** of the threshold, and browsing `SQ:`
with it still does not fault (oracle: 235 samples, 0 downs).

That one byte is now the whole problem for 2.1c: either a field with a larger cap exists, or the guard is
reached some other way.

### The copy is object-generic — the "other field" is the same slot on other objects

`0x101cc878` takes the object as `r3` and copies `r3+4212`/`r3+4208`, gated by the byte at `r3+4216`
(`0x101cc8d8`). It is called with five different objects: `0x101ce814` passes `r31+28`, `0x101cebc4`
passes `r20`, and `0x101ccd7c`/`0x101cd85c`/`0x101cec24` pass their own. So there is no second field
inside the object — the same `+4208/+4212/+4216` triple is copied for whichever object is handed in.

The only writer is the setter `0x101cbb38`, and it has ten callers, each a different object:
`0x101cbd78` (`r28+19684`), `0x101cd168` (`r27`), `0x101cdb5c` (a stack object), `0x101da6c8` (`r22`),
and six more. The wire actions behind them are `SetAVTransportURI` (`0x101ce388` → `0x101cbc08`) and
the queue-add path; `SetNextAVTransportURI` is reachable but answers 701 unless a track is playing.

So the blocker is not the field, it is the validators: `SetAVTransportURI` 714s past ~3800,
`AddURIToQueue` 804s past ~1000, and the queue actions 800 on a slave. Any of the ten setter callers
would do if one of its actions accepted a >2100 string.

### 2.1e CreateSavedQueue works on the coordinator with real DIDL metadata (2026-10-26)

Queue actions go to the **coordinator**: `.84` answers, `.202` is the right channel of a stereo
pair. The 811 was the missing `EnqueuedURIMetaData` — with a real DIDL-Lite item there,
`CreateSavedQueue` on `/MediaRenderer/AVTransport/Control` returns **200**:

```
Title "T1"          -> 200, NumTracksAdded 1, AssignedObjectID SQ:1
Title 2046 x 'A'    -> 200, AssignedObjectID SQ:3
Title 2139 x 'B'    -> 200, AssignedObjectID SQ:6, daemon alive
```

So a >2 KiB title is accepted and stored. Caveat measured immediately after: browsing `SQ:6`
with `BrowseMetadata` returns the title **truncated to 1023 bytes**, so the stored value is
longer than what the browse path hands back — which is exactly the serializer/reader split that
matters for the 2.1c NUL and for any title-driven overrun.

### The 2 139-byte span is accepted, and the setter guard is not what the doc said

Re-read of `0x101cbbbc`: it is `cmplwi 7,3,1024` followed by **`bt 2`** — branch if *equal*. So the
store is skipped only when the span is exactly 1 024, not "unless it exceeds 1 024"; 1 023 is stored
normally. The cap on titles (1 023 bytes) therefore does not block the setter at all.

Live on the coordinator: `SetAVTransportURI` with a 2 139-byte share (`x-file-cifs://host/<2139 A>/x.mp3`,
span 2 139) answers **200** and the daemon survives (`zp` 200). So the copy runs with the span that
zeroes the caller's `r31` — the corruption is silent rather than fatal, which is what a low-byte zeroing
of a still-mapped pointer looks like.

## 2026-10-27 H14 (bead frankensonos-1w3): the span-2139 NUL is a PC-control chain

Caller of the splitter loop resolved: `0x101cbfe4` in `fcn.101cbe80` (frame 0x2560)
constructs the iterator at **`sp+0x1520`** (`addi r29, r1, 0x1520` → `0x1019fda0`:
`{0, r4, r5, 0}`, FILE* filled at +0 by `0x1019fe30`) and passes it as `param_3`
(`r5`) to `0x101a016c`, which saves it as its `r31`.

The dispatch after the splitter returns is confirmed at instruction level
(`0x101a0224`): `lwz r9, 0(r31)` / `lwz r9, 0xc(r9)` / `bctrl`, re-dispatched every
loop iteration (also `+0x10` at `0x101a01d4` and `+8` at `0x101a026c`). The span-2139
NUL zeroes the low byte of that saved `r31`, so every later dispatch runs on
`(sp+0x1520) & ~0xFF`. *(Corrected 2026-10-28: it is the **MSB**, and the copy is
unbounded — see the correction section at the end of this file.)*

Frame map of `fcn.101cbe80` (all offsets from its own `sp`):

| offset | content |
|---|---|
| +0x10 | URI object (setter `0x101cbb38` target, `r30`) |
| +0x1090 | scratch (copied toward +0xe region) |
| +0x10c0 | decoded host buffer (`r27`) |
| +0x14d0 | decoded name buffer, 0x40 bytes (`sonosDecodePercentEncodedStr`, `r28`) |
| +0x1500..0x150F | **name-buffer bytes 0x30..0x3F** |
| +0x1510 | lookup key (16 B, `sth 0` at +0x1510) |
| +0x1520 | **the iterator (`r31` of `0x101a016c`)** |
| +0x2530/0x2534 | ints; +0x2548..0x255C saved regs |

Consequence: `(sp+0x1520) & ~0xFF` = `sp+0x1500` exactly when `sp ≡ 0x00 (mod 0x100)`
(otherwise `sp+0x1400..0x14F0`). At `sp+0x1500` the "vtable pointer" fetched by
`lwz r9, 0(r31')` is **decoded-name bytes 0x30..0x33 — attacker bytes** (percent-decode
output). The chain is then `lwz r9, 0xc(r9)` → `bctrl`: PC = `*(fake_vtable + 0xc)`,
with `fake_vtable` itself pointing back into attacker-controlled stack bytes.

So the full chain is: one unauthenticated `SetAVTransportURI` with a `x-file-cifs`
URI whose share-path span is 2139 → NUL zeroing → fake vtable from decoded-name
bytes → `bctrl` to a chosen address. No heap groom needed — the fake vtable and
(if `fake_vtable` is aimed back into the name buffer) the final target are both
the same 64-byte decoded buffer.

Open unknowns, all cheap:
1. `sp mod 0x100` at `fcn.101cbe80` (constant per call path; read once via a
   crash-address or qemu).
2. Whether `sonosDecodePercentEncodedStr` can emit `0x00` bytes (else the fake
   vtable pointer can't contain a zero byte — affects which PLT addresses are
   encodable).
3. Device confirmation: span-2139 run with name bytes 0x30..0x33 = unmapped vs
   mapped pointer → crash/behavior delta proves the redirected dispatch fires.

## 2026-10-27 H14 device round 2: the setter path is HTTP, the chain is via `abstract`

Raw-sniff of the setter's outbound connection (`x-file-cifs://host:port/S`):
the client sends **`HEAD /S HTTP/1.1`** (User-Agent `Linux UPnP/1.0 Sonos/`) —
anacapad's own "CIFS" client speaks **HTTP**, not SMB. Every earlier "1 s close
with no SMB" observation was this HEAD probe. The libsmb2 client belongs to the
`/getaa` art pipeline only; the two SMB stacks never mix.

`fcn.101cbe80`'s iterator FILE comes from `FUN_1019fe30` → `snprintf(buf, 0x400,
"%s/abstract", …)` → `fopen` — the client downloads **`<share>/abstract`** (HTTP)
and iterates `{u32, u32}` records (`FUN_101a000c`: `fread(&local_18, 8, 1)`,
`FUN_10072914(local_18)` → `FUN_103897b8(..., "x-file-cifs://" + …, 0x401)` per
entry). So the whole chain is HTTP-downloadable content:

```
SetAVTransportURI x-file-cifs://host:port/SEG2139/x.mp3
  → anacapad HTTP client: HEAD /SEG2139  (probe)
  → GET /SEG2139/abstract                (record file)
  → fcn.101cbe80: 0x101cbb38 stores the URI (span = len(SEG2139) — 2.1e's
    measured span equals the raw share length; host:port validated on .84)
  → 0x101a016c loop: per record, splitter 0x101cc878 copies obj+4212/4208
    into a 2160-byte frame; the terminator NUL at sp+span+17 = sp+2139+17
    zeroes saved-r31's low byte (the map's span-2139 slot)
  → restored r31 = (sp+0x1520) & ~0xFF → vtable fetch from attacker-influenced
    stack window (decoded host buffer 0x10c0..0x14C1 budget 0x401, or decoded
    name 0x14d0..0x150F, or key 0x1510) → lwz/lwz/bctrl at 0x101a0224
```

Corrections this round:
- The morning's "SetAVTransportURI 200 + full SMB chain" reading was wrong — that
  chain in the impacket log was the concurrent `/getaa` fetch. The setter never
  spoke SMB; "silent r31 zeroing" from 2.1e was inferred, not observed, and the
  copy likely never ran (no `abstract` file existed on any probed share).
- span == raw share-segment length (validated: 2136/2139/2100-char segments all
  200 with port-in-URI; the URI parser accepts `host:port` and the client dials it).
- The HTTP probe is `HEAD /<share>` — the share name is sent raw (2118-char share
  accepted by impacket; the HEAD probe carries it as the path).

Remaining unknown (single): the `abstract` record format (what `FUN_10072914`
consumes — likely `{u32 offset-into-file, u32 len}` or NUL-terminated string
pairs; readable from `FUN_10072914` / the writer side). Then the device test is:
serve `HEAD` + `/abstract` with a record whose encoded entry URI span lands the
NUL in the saved-r31 slot, tails mapped vs unmapped → crash delta = PC-control
proof; iterate tail bytes toward a chosen `bctrl` target.

### `FUN_10072914` decoded: abstract records are keys into a 0x400-slot pointer table

`FUN_10072914(key)` = `0x101a2b3c(0x105480DC, key)`: `key == 0x400` → fixed object
`0x1054FDA8`; otherwise `ptr = *(0x105480DC + 4) + key*4` (a runtime pointer
table, 0x400 slots, `*(tbl+4)` = array base). `FUN_10072940(key, 1)` →
`0x101a2768(tbl, key, 1)` (getter with flag). So each 8-byte abstract record is
`{u32 host_key, u32 name_key}` — indices into an interned-string table the CIFS
client populates when it mounts/lists a share. The remaining work is
mechanical: identify the intern keys the mount assigns (dump the table after a
live mount — the guest qemu monitor can read `0x105480DC`), serve a matching
`abstract`, and run the mapped-vs-unmapped tail discriminator.

### `FUN_101e474c` decoded: `/abstract` is a DOWNLOAD, written to a local cache

Decompiled the second `%s/abstract` user (`0x101e4a3c`, inside `fcn.101e474c`,
called only from `fcn.101e4a84` ← `fcn.1007fef4` — the anacapad own-client fetch
pipeline). It is a mode-dispatched stream pump:

| mode | behavior |
|---|---|
| 0 | `FUN_10079f30(obj[0x2152], buf)` — push bytes into ring A (`0x1038ed9c`), flag at obj+0x1DAA |
| 1 | `FUN_10079f58(obj[0x2152], buf)` — push bytes into ring B (obj+0x1040), same flag pattern |
| 2 | `open(DAT_10545c38 + "/abstract", O_WRONLY\|O_CREAT, 0600)` + `write()` the received stream |

All three receive via vtable `+8` (recv with a +5 s deadline) and size-check via
vtable `+0x30`, in ≤0x19000-byte chunks (4-byte-aligned when `param_4` set).
`DAT_10545c38` is the local cache directory global.

Consequence: **the `abstract` file the iterator reads is downloaded verbatim from
my server** (mode 2 saves exactly the bytes sent) — no on-device format
transformation on the write path. The intern-key semantics live on the parse
side (the ring consumer pump, one level deeper), but the DELIVERY half of the
chain is now fully pinned: serve `HEAD`, serve `abstract` bytes; the device
caches them locally and `FUN_101a000c` `fseek`/`fread`s the cache as
`{u32,u32}` records.

### The `/abstract` file format decoded (iterator side)

`FUN_1019feb8(itr)` (called at the top of `FUN_101a000c`) reveals the layout:

```
itr[0] = FILE*            (the locally cached <share>/abstract)
itr[2] = record limit
itr[3] = next entry index
itr[4] = offset-table base (file offset)
offsets[] = 0x400 × u32 at file offset itr[4]
```

Seek: `fseek(FILE, itr[4] + (itr[3] & 0x3ff) * 4, SEEK_SET)` → `fread` 4 bytes =
the entry's file offset; remaining-records guard is `itr[2] - itr[3] ≤ 0x400`.
`FUN_101a000c` then reads the 8-byte record at that offset: `{u32 a, u32 b}`,
each passed to `FUN_10072914` → `0x101a2b3c(0x105480DC, key)` → pointer-table
lookup → a string/intern object whose bytes `FUN_103897b8` copies into the
entry URI buffer (`"x-file-cifs://" + host + "/" + name`, 0x401 cap).

So `/abstract` = `[u32 table_offset][u32 limit][offset table…][records…]` (exact
header order still to confirm from the producer side), records hold intern keys,
and the intern table is populated by the mode-0/1 ring pumps fed from the same
fetch (`FUN_101e474c` modes 0/1 → `FUN_10079f30/58` → `0x1038ed9c` byte rings on
`obj[0x2152]` / `obj+0x1040`).

Remaining: (a) which bytes the mode-0/1 pumps expect (string-blob section
format) so the intern table gets OUR strings; (b) the pump that then assigns the
keys and fills the iterator's offset table — the writer-side sibling of
`fcn.1007fef4`. Both are static-readable from the fetch object's vtable
(`0x10080000`: `bctrl` via `*(obj)+0xe4`, then `FUN_101e4468` / `FUN_101e4548`).

### Cache locations + lifecycle (2026-10-27, continued)

- `DAT_10545c38` (abstract write side, `FUN_101e474c` mode 2) = **`/jffs/shadow2`**;
  the read side (`FUN_1019fe30`, `0x10545c40`) = **`/jffs/shadow`**. Both are
  `strdup`ed at init (`0x1000d3c8`).
- `fcn.10072590` / its sibling = `sonosRmDirRecursive`+`sonosMkDirRecursive` on
  each cache dir — a cache-reset helper called from the **sharelist index**
  machinery (`fcn.1007f868`, strings: `"sharelist"`, `"need to recover ix=%d"`,
  `"removing index before re-indexing"`). So `/jffs/shadow{,2}` hold the
  **sharelist index cache**; the reset fires on re-index/recovery paths.
- The fetch object constructor `FUN_101e4468` installs vtable `PTR_FUN_10478a98`
  (slot +8 = `0x1035d160` recv-with-deadline, +0x30 = `0x1035b778` size check)
  — the stream class behind `FUN_101e474c`'s mode pump.

Net: the attacker-served `/abstract` lands in a **cache** directory whose
lifetime is governed by the sharelist re-index cycle — persistence across
reboot is conditional (cache reset timing), not guaranteed. For the H14 chain
persistence doesn't matter (single-shot SetAVTransportURI → fetch → iterate →
NUL happens within one request), but for any future "persist a file on /jffs"
lane the reset conditions need mapping.

### Wire-confirmed end-to-end + the sharelist cache (2026-10-27, final round)

Served `HEAD` + `GET /abstract` over HTTP on a fresh port; the device's setter
path hit **both** (`HEAD /<SEG2139>`, `GET /<SEG2139>/abstract`), parsed the
served bytes, and immediately fetched an **entry** (`/folder.jpg`) — the chain
runs end-to-end from one unauthenticated `SetAVTransportURI` to attacker-fed
record parsing. Follow-up probes on the same/fresh ports stayed silent: the
pipeline runs on the **sharelist re-index cycle**, not per-setter; the cache
(`shares.xml`, `.version` stamp, `/tmp/smb/spt_%d_%u` index files — strings at
`0x1044f700`–`0x1044f8f0`) short-circuits repeat probes until re-index/recovery
(`"Rejecting invalid sharelist"`, `"removing index before re-indexing"`).

Remaining engineering to PC control (single unknown left: make the served
abstract carry intern-table keys that resolve to attacker strings):
1. Producer pump analysis (mode-0/1 ring consumers) OR qemu live dump of
   `0x105480DC` after one real fetch — learn the key→string assignment.
2. Serve an abstract whose records reference keys that decode to attacker
   strings; entry URIs then carry controlled bytes through the splitter window.
3. Mapped-vs-unmapped tail discriminator → crash delta → aimed `bctrl`.

### Probe-gate status (2026-10-27 evening)

After the one successful end-to-end fetch, the device stopped dialing entirely —
new ports, fresh share names, getaa tickles, and Play all return 200 without any
outbound connection (verified: listener live, `wc -c` 0 on server logs across
~15 attempts over ~40 min). The gate is stateful inside anacapad (sharelist
index + likely a per-host cooldown), not URI-keyed. `.86` rejects the same
setter with UPnPError 1023 (different setter path on S2 — its own RE task).

Trigger candidates for the next session, in order of cheapness:
1. Map the re-index trigger: `fcn.1007f868`'s recovery branch needs
   `0x1007c39c(*(obj+0x5110)) != 0` — find what sets `obj+0x5110` (likely a
   "shares changed" flag from the CIFS registration path).
2. The indexer worker `fcn.100800b0` (frame 0x1e180) — read its entry conditions;
   it is invoked from the sharelist state machine (`0x100809a0` region).
3. Device reboot of `.84` (power-cycle or updater path) clears in-memory state —
   guaranteed fresh window for the discriminator, needs owner approval per
   device discipline.

Once any of these reopens the fetch window, the discriminator is a single
served-abstract swap: mapped tail (`0x105464b0`) vs unmapped tail (`0x41414141`)
at the saved-r31 window → crash delta → aimed `bctrl`.

### 2026-10-27 evening: the HEAD probe gates EVERYTHING — dual-mode discovery

Raw sniff resolved the day's contradictory observations. The `/getaa` art
client **also** sends `HEAD /S HTTP/1.1` before any SMB — the HTTP probe is
universal for `x-file-cifs` sources. Device behavior splits on the probe:

| HEAD probe result | client behavior |
|---|---|
| **200** (any HTTP server) | pure-HTTP art fetch: `GET /S/f.jpg`, `GET /S/folder.jpg` — **no SMB2 at all** (observed: 4 shots, all HTTP) |
| **fail/close** | this morning: SMB2 fallback ran once (full NEGOTIATE→READ chain). **Now:** client gives up in ~1 s, no SMB2 — the sharelist cache has since registered the source as probed |

Consequences:
1. The morning's "SMB2 chain via getaa" ran during the **first-ever contact**
   with that host:port (cold sharelist). After registration, the HTTP probe
   result is cached and the SMB2 fallback no longer engages.
2. My crash ladder (`--fndelta 45045`, above the computed `buflen + 24576 =
   90111` threshold from RE_CVES §7c — prior runs used 148/32788/16286/32670,
   **all below it**) never reached the SMB2 QUERY_INFO parser because the
   client never left HTTP mode.
3. The dual HTTP+SMB server works (built and committed); it just can't help
   while the client is in cached-HTTP mode.

Reopen paths for the SMB2 fallback (in cost order): map the sharelist
re-index trigger (`fcn.1007f868` recovery branch / what sets `obj+0x5110`); read
indexer `fcn.100800b0` entry conditions; or reboot `.84` (owner approval —
cold sharelist guarantees the morning's fallback window).

### The sharelist indexer state machine decompiled (`fcn.100800b0`)

Loop structure (per cycle): wait on flags (`param+0x4d09/0x4d0a/0x4ce4`) →
"processing index" → validate each sharelist entry (`FUN_1007cc3c` parse +
`FUN_1007be14` validate, entries at `param+0xa0`, stride **0x4c4**) →
replication check → per-share indexing `FUN_1007d21c(obj+0x5110, …)` →
`FUN_1007fef4` (fetch) inside → failed entries marked, "indexing reported
err=%d for %s" → **`FUN_100725c8()` = the `/jffs/shadow2` cache reset runs at
the END of every cycle** → `.version` state flags cleared.

Consequences:
1. `/jffs/shadow2/abstract` is **ephemeral** — wiped after each index cycle;
   persistence across cycles needs the abstract to be re-downloaded each time
   (fine — the fetch happens inside the same cycle before the reset).
2. The sharelist entries (0x4c4-byte records at `param+0xa0`) are parsed from
   **`/jffs/shadow/shares.xml`** (`FUN_1007cc3c`) — the device's own share
   registrations. Setter-driven registrations feed this file.
3. The per-share indexer `FUN_1007d21c` is the parent of the mode-0/1/2
   download pump (`FUN_101e474c`) — the listing parse (rings → intern table)
   and the abstract fetch both hang off it.
4. The gate observed on the wire (no re-fetch after the first) = the indexer's
   per-cycle dedup: shares already indexed in this cycle are skipped unless
   the re-index flag (`param+0x4d07`/`0x4ce8/0x4ce9` dance) is set.

### 2026-10-27 night: the two lanes converge — fcn.101a0b6c stack copy

`fcn.101a0b6c` (frame `0x450`) does `strrchr(path,'/')` → `memcpy(r1+0x10,
lastslash+2, seg_len)` — **an unbounded path-segment copy into a stack frame
whose saved r24–r31 sit at +0x428..0x450 and LR at +0x454**. A path segment
longer than ~1040 bytes smashes the return address directly. This is the 9sd
"stack copy not on the SetAVTransportURI path" — but its caller chain
(`FUN_101a1c84` ← data `0x1046f8ac`, the ContentDirectory/browse area, sibling
of the splitter chain `fcn.101a0b6c` ← same `0x1007ad60` CIFS-fetch family)
is exactly where **sharelist-registered paths** flow.

The 9sd live test drove it via SetAVTransportURI (wrong path → "no crash").
The correct driver is the library/browse enumeration of a share whose LISTING
came from my server — i.e. the H14 registration chain, then a browse.

Root-shaped synthesis (all links individually proven):
1. setter/getaa registers the CIFS source (proven)
2. sharelist indexer fetches the listing/abstract from my HTTP server (proven)
3. stored paths flow into the browse/library enumeration (fcn.101a1c84 family)
4. `fcn.101a0b6c` copies the last path segment unbounded into a 0x450 frame →
   **saved-LR control with attacker bytes** (static; length unbounded, no clamp
   found by 9sd's sweep)

Next concrete steps: (a) confirm the data path from a sharelist listing to
`fcn.101a0b6c`'s `r4` argument (static, one decompile: `FUN_101a1c84`); (b)
serve a listing whose file names carry a 1100-byte final segment; (c) browse
the share via ContentDirectory; (d) observe the LR smash.

### 2026-10-27 night: FUN_101a1c84 decompiled — the abstract-path stack copy is BOUNDED

`FUN_101a1c84` (the "itplist" per-track processor) confirmed as the abstract
consumer: bsearch over the abstract's offset table → track URI (param_3, my
served bytes) → `sonosDecodePercentEncodedStr(uri, local_1070, 0x401, …)` →
`FUN_101a0b6c(obj→+0xf4, decoded, …)` — the unbounded stack-copy function.

**But the copy is bounded on this path**: the decoded URI is capped at 0x401
(1025) bytes, the last-segment copy therefore ≤ ~1008 bytes, and the 0x450
frame has 1048 bytes of runway before saved r24. The 9sd 4000-byte test that
"landed on slack" was the SetAVTransportURI caller path (no 0x401 cap); the
abstract path cannot exceed the decode cap. The LR smash via this specific
route is **not reachable** — honest negative, the chain closes here.

What remains un-audited on the abstract path: the mode-0/1 ring pumps (the
listing bytes parsed during download — a different parser from the iterator),
and the second `FUN_101a0b6c` call inside `FUN_101a1c84` uses the same bounded
buffer. The unbounded-copy risk stays with the SetAVTransportURI-side callers
of `fcn.101a0b6c` (0x101ccc04 region — the splitter family), where the
2.1c/2.1e NUL primitive already lives.

### 2026-10-27 night: re-index trigger identified — cloud config sync

The scheduled-reindex flag writers (`0x4d07/0x4ce8/0x4ce9`) live in
`fcn.1007df60` ("schedule full reindex"), reached from `fcn.1007e150` ←
`0x10128330`, whose dispatcher `strcmp`s **`presentationMapVersion` /
`presentationMapUri`** argument-layout strings (music-services config sync).
The re-index — and with it the `/abstract` fetch and the SMB2-fallback window —
is driven by **cloud music-service configuration changes**, not by LAN
requests. (`SetZoneAttributes` was probed as a candidate carrier: 200, but it
carries ZoneName/Icon/Configuration, not the presentation-map arguments.)

Reopen routes, final: (1) device reboot of `.84` (owner approval; cold sharelist
guarantees the fallback window); (2) a music-services config change (cloud
side); (3) deeper RE of the presentation-map sync handler for a LAN-reachable
sub-path. All infrastructure and probe scripts committed and reusable.

### 2026-10-27 night: S2 crash ladder results (bead 1w3, getaa vehicle)

Ran the FileNameLength ladder on **.86** (S2 — whose getaa dials SMB2 directly,
no HEAD gate) with the committed `nc0drop.py`:

| configuration | declared FileNameLength | result |
|---|---|---|
| `--fndelta 45045` | 90100 | full chain ran; **worker pinned** (3 shots "closed without response"), daemon recovered |
| `--pad 16384` (2-byte margin, 16KiB body) | 16286 | 200, survived |
| `--pad 32768` | 32670 | 200, survived |
| `--fndelta 60000` | 120020 | full chain; **worker pinned** (2 shots), recovered |

The over-read demonstrably **runs 90–120 KB past the 64 KiB client buffer** on
the live S2 daemon and the heap absorbs it — no fault through this vehicle yet.
The pin is the documented pre-auth DoS; the crash needs the receive-buffer's
heap neighborhood groomed so the walk hits an unmapped page (allocator
feng-shui on the art worker — next root-shaped work item). Also confirmed:
the S2 getaa SMB2 path has **no HEAD gate** (dials SMB2 directly, repeatable
per request) — unlike .84 whose getaa is now HEAD-gated with a cached fallback.

PresentationMap sync verdict (this round): the re-index scheduler is fed by
`fcn.1007e150` ← `0x10128330` whose dispatcher matches
`presentationMapVersion`/`presentationMapUri` argument names — a **cloud
music-services config** event, not LAN-reachable. `SetZoneAttributes` probed
(200) but carries different arguments.

### 2026-10-27 night II: S2 art-cache negative marking + worker pool

After the pin runs, `.86` began returning instant 404s for new art names on the
same source (negotiate offers visible but the chain stops before QUERY_INFO;
QINFO patch count 0). The art cache **negatively marks the source after failed/
pinned fetches**; recovery took >1 min of cooldown. This matches the .84-side
probe-gate behavior — the art cache marks per-source state, and the worker pool
recovers slowly after pins.

Practical consequence for the crash ladder: each ladder rung needs (a) a fresh
source key (host:port variant) OR (b) a post-pin cooldown, and (c) a warm-up
shot (the first shot after cooldown often 404s before the pool is warm —
matches "2-3 shots per run" in the prior lane's recipe).

### 2026-10-27 night III: pin is REPEATABLE on .86 (final ladder data)

With the art-cache expired (`f.jpg` re-fetchable), the ladder at
`--fndelta 60000` (declared 120020): **shot0 pinned** (connection closed, zp=0 —
worker walked 120 KB past the buffer), shots 1–2 recovered to 200. The QINFO
patch fired 4× (root + file queries across the shots). Also decoded the abort
mode: requesting a NON-EXISTENT file name aborts the chain right after
`Create` (the client never reaches the file's QUERY_INFO) — ladder shots must
request files that exist on the share.

Session total for the S2 crash lane: the over-read is **live, repeatable, and
 controllable** (declared length fully attacker-chosen per request via the
committed `nc0drop.py --fndelta/--pad`); the daemon survives via worker-pin +
recovery. Converting pin → fault = heap grooming: control what the allocator
places after the 64 KiB receive buffer so the 120 KB walk hits an unmapped
page. Concrete next levers: (a) concurrently allocate/free large art buffers
to shape the top chunk; (b) test at the exact threshold band (90111 ± small)
where the walk ends just past a freshly-mmap'd region; (c) audit
`FUN_101a3114`/the getaa handler's allocation order (the buffer's neighbors
are deterministic given the request sequence).

### 2026-10-27 night IV: READ reply injection — attacker PDU delivered past the desync boundary

Upgraded `nc0drop.py --inject <frame>`: the READ reply's Buffer is padded to
the client's requested 65535 bytes, then the crafted NetBIOS-framed PDU is
appended, `DataLength = 65535 + len(frame)`. Per RE_GETAA_DATALENGTH's
desync mechanics (`DataLength` returned as payload length with no comparison
against the requested count; surplus stays in the socket; next parse starts
mid-stream), the injected frame lands **exactly at the position where the
client's parser resumes**.

Wire result on `.86` (3 shots): the full SMB2 chain ran, 65629 bytes
delivered, the art worker's HTTP response aborted mid-stream
(`IncompleteRead`) — the desync happened on schedule with attacker bytes at
the boundary. The client-side parse of the injected PDU is the next
verification: the frame must carry the session's mid/sid/credits to be
accepted (a valid second READ reply or an ECHO), after which **arbitrary
SMB2 reply injection inside the established session** is proven — the
strongest primitive short of memory corruption on this chain (reply
injection into the art worker's SMB client, attacker-controlled message
type and payload).

Infra: injection frame builder at `/tmp/h14_inject_build.py` (committed
next), `nc0drop.py --inject` flag committed.

### 2026-10-27 night V: mid-following reply injection DELIVERED

`nc0drop.py --inject --midoff N`: at each READ, the patch extracts the live
request's `MessageID/SessionID/TreeID` from `recvPacket`, builds a complete
second READ reply (`build_followup_read_reply(mid+N, sid, tid)`, payload =
attacker bytes), pads the real payload to 65535, appends the frame, and sets
`DataLength = 65625`. Wire result on `.86`, 3 shots: **65625 bytes delivered
per request with the attacker frame at the desync boundary**; mids tracked
per-request (9→12, 13→16, 17→20 with `--midoff 3`).

Status: the injection is mechanically proven (attacker PDU delivered at the
exact parse-resume position, correct session context). The client-side parse
of the injected frame is not yet observable through the art handler (its HTTP
response aborts at the desync — `IncompleteRead` — before any injected
content would surface). Verification options: inject a valid JPEG-header
payload and watch for it as the art response body on the recovery path;
inject an ERROR-status reply with the client's mid and diff the getaa error
code; or trace the client's post-desync socket state via the device side.

This is reply injection into the art worker's SMB client inside an
established session — attacker-chosen message type, mids, and payload — the
strongest primitive on this chain short of memory corruption.

### 2026-10-27 night VI: payload injection fires per-READ — double-patch identified

`--payload` flag added (payload file → `build_followup_read_reply` body).
Wire result: injection fires on EVERY READ in the chain (f.jpg read mid 9→12,
folder.jpg read mid 12→15) — the patch patches the folder.jpg READ too,
producing TWO mid-12 frames (one from the f.jpg desync surplus, one patching
the real folder.jpg READ). The art worker's HTTP response aborts
(`IncompleteRead`) on all shots — the marker payload doesn't reach the HTTP
layer because the worker's response dies in the double-injection confusion.

Next refinement: single-shot injection per connection (patch only the FIRST
READ; skip subsequent) so exactly one injected frame lands, answering the
folder.jpg READ; then the art worker's flow may complete and deliver the
marker payload to the HTTP layer.

### 2026-10-27 night VII: MARKER — attacker payload delivered THROUGH the device

`nc0drop.py` second-READ replacement mode: the reply to the folder.jpg READ
(mid 13) is replaced with `PAYLOAD` (correct DataLength, no desync). Wire
result on `.86`: **the getaa HTTP response body contained the injected
payload** (`H14INJECTEDMARKER` found in the 200 response) — the device
fetched my injected bytes via SMB2 and served them to the requesting HTTP
client.

Chain: unauthenticated `getaa` → SMB2 session to my server → second READ
reply replaced with attacker payload → device delivers attacker content.

Root significance: the art handler **decoded my served JPEG** (49-byte valid
JPEG header+EOI with marker) — meaning the **JPEG decoder now processes
fully attacker-controlled data from an unauthenticated request**. The JPEG
decoder was the f4k lane's un-audited surface; it now has a live delivery
primitive. Any decoder bug = pre-auth code execution candidate with a
complete delivery chain.

### 2026-10-27 night VIII: no on-device JPEG decode via getaa — raw serve + default art

Systematic checks: valid PIL-generated JPEGs (826B red, 2074B blue, fresh
names) → the device serves either the RAW file bytes (folder.jpg case:
RAW-SERVE md5 match) or a **fixed 325B default art blob** (md5 3e8ea497,
stable across all malformed/valid inputs). The apparent "transform"
(d5dacb00) was a transient cache artifact, not reproducible. 17-case
malformed-JPEG fuzz (truncations, marker corruption, dimension overflow,
segment-length overflow, random mutations): **0 crashes**, all served the
default. The w/h parameters do not change the output.

Conclusion: `getaa` **serves art bytes raw** — there is no on-device JPEG
decode in this flow (the decode happens on the requesting controller). The
attackers-bytes-through-the-device delivery primitive stands (raw serve of
attacker content), but the "JPEG decoder as pre-auth code-exec surface"
thesis is dead for this path. The delivery primitive's value: attacker
content served from the device's HTTP surface, and the art cache as a
state container.

### 2026-10-27 night IX: .86 daemon restart observed (SWGen 1→2) — trigger unidentified

`.86` SWGen = 2 (one daemon restart during today's testing; `.84` = 1).
The restart happened at some point during the ladder/concurrency runs; the
exact trigger run is unidentified. Candidates: the 8-concurrent fndelta-60000
wave (all 8 workers pinned simultaneously, zp=0 listener-wide), or the first
solo 120020 walk. Repeatability attempts FAILED: the 8-concurrent wave re-run
pins all workers but SWGen stays 2 (no restart); the desync concurrent wave
didn't pin (all 200s fast — the art cache answered/aborted early).

State of the crash lane: a daemon restart WAS observed once today on .86
(real fault), the leading recipe is the 8-concurrent over-read wave, but it
is not yet repeatable. The cooldown/cache-marking behavior complicates the
repeatability ladder (each wave needs fresh hostnames + cooldown).

### 2026-10-27 night X: concurrency experiments — the restart trigger remains elusive

Session-total: ONE daemon restart observed (SWGen 1→2), trigger run
unidentified. Final concurrency experiments: 4×fndelta-60000 walks (360KB
total over-read, no fault, one 200-completion at 30.7s, three 46s pin-drops);
4×desync-65535 (3×60s timeouts = desync retry-loop pins, no READs reached —
the chain aborted at the share-root query). The desync+concurrent+fresh-names
alignment resisted the art-cache timing across 6 attempts.

The restart's most probable recipe (from the day's evidence): the desync runs
(--dlen 65535) were ACTIVE during the SWGen 1→2 transition — the desync's
stream corruption (surplus bytes parsed as the next PDU) is the chaotic
element the over-read walks lack. The next session's decisive experiment:
desync + fndelta COMBINED on the same server (READ reply with BOTH surplus
desync AND a padded FileNameLength over-read in the follow-up QueryInfo —
the two primitives chained in one session), fresh names, 4-8 concurrent.

### 2026-10-27 night XI: audio codec attack surface — FLAC/Vorbis/AAC identified, delivery proven

**Codec inventory (from anacapad binary strings):**
- libFLAC 1.3.4 (20220220) — statically linked, `FLAC__STREAM_DECODER_*` symbols present
- libVorbis (Xiph.Org 20200704 = 1.3.7) — `vorbis_packet_blocksize` etc.
- Custom AAC parser — `(%d) corrupt file:` error string (Sonos's own code)
- MPEG-TS demuxer — proven working via hls_det.py all day

**Known CVE status:**
- libFLAC 1.3.4: CVE-2020-22219 (bitwriter_grow_, ENCODER only — decode path unaffected).
  All DECODER CVEs (2014-8962, 2014-9028, 2017-6888, 2020-0499) are fixed by 1.3.4.
  No known DECODER CVEs against 1.3.4. BUT: OSS-Fuzz continues finding bugs in
  1.3.x decode paths that never got CVEs. The 1.4.x changelog has decode-path
  fixes that 1.3.4 lacks.
- libVorbis 1.3.7: all listed CVEs (2007-2018) fixed by 1.3.7. No known vulns.
- The custom AAC parser: Sonos proprietary, zero public audit. Highest bug-density target.

**Delivery:** SetAVTransportURI + Play is unauthenticated and proven (hls_det.py).
The device fetches the stream URL and decodes in-process. The decode path
(flac/vorbis/aac) runs inside anacapad = pre-auth reachable.

**Fuzz round 1 (HLS + malformed WAV in TS):**
- `fuzz_wav_huge.ts` (WAV with 0x7FFFFFFF RIFF/data sizes): transport died at 12s
  (DEAD state) — the device dropped the stream on the malformed content.
  Not a daemon crash (SWGen unchanged), but the decode path hit an error.
- `fuzz_audio_rand.ts` (random bytes): PLAYING, 0 segments fetched (art cache
  blocked the source). Not a clean test.
- Repeat attempts: blocked by the art cache negative marking (cooldown needed).

**Next session:** sustained codec fuzz with the hls_det vehicle + cooldown
discipline + the custom AAC parser as the primary target (no public audit,
proprietary code, highest bug density). FLAC/vorbis as secondary (statically
linked, older lib versions, but 1.3.4/1.3.7 have most public bugs fixed).

## 2026-10-28 H14 correction: the splitter is an UNBOUNDED stack copy — saved-LR control, not just a NUL

The 2026-10-27 note's frame map is right (frame 2160, `stw r30,2152`, `stw r31,2156`,
`stw r0,2164`), but its terminator arithmetic is off by three bytes and it understates the
primitive: the copy itself has **no bound check against the frame**.

Exact code, `0x101cca44`–`0x101cca68` (inside `0x101cc878`):

```
101cca44  subf  r29,r3,r27      ; span = strrchr(uri,'/') - strchr(uri+prefix_len,'/')
101cca48  addi  r22,r1,16       ; dst = sp+16
101cca50  addi  r5,r29,1        ; len = span+1
101cca58  bl    memcpy
101cca60  add   r9,r1,r29
101cca64  li    r0,0
101cca68  stb   r0,17(r9)       ; sp[span+17] = 0
```

`prefix_len` is `obj+4208`, set by `0x101cbb38` to `strchr(uri+14,'/') - uri + 1` — the offset of
the first `/` after the scheme+host. So the copied bytes are the raw URI share path
(`/` + share + `/`), attacker bytes, and the destination runs straight through the saved
register block:

| copy byte `i` lands at | saved slot | overwritten when |
|---|---|---|
| `sp+16+i` | — | — |
| 2136..2139 | `r30` (2152..2155) | span ≥ 2139 |
| 2140..2143 | `r31` (2156..2159) | span ≥ 2143 |
| 2148..2151 | **saved LR** (2164..2167) | span ≥ 2148 |

Consequences, and the corrections:

1. **span 2139 zeroes the MSB of saved `r31`, not the LSB.** Big-endian: `sp+2156` is the top
   byte. The 10-27 note's "low byte" is wrong. The LSB needs span **2142**.
2. **The copy already writes attacker bytes into `r30` at span 2139** (last copied byte =
   `sp+2155` = `r30` LSB), so the 2.1e run was not a pure NUL either.
3. **span ≥ 2148 reaches the saved LR** (if the URI is not truncated first — see unknown 1),
   and span 2151 overwrites all four LR bytes with URI bytes (the NUL then lands at `sp+2168`,
   in the caller's frame). The epilogue restores `r0` from `sp+2164` and `mtlr r0; blr`
   (`0x101cc8e4`), so the splitter returns straight to the attacker address. Share offsets
   2147..2150 become the return address.
4. That makes the primitive **direct saved-LR control**, not a NUL plus a vtable indirection:
   the `0x101a0224` dispatch (`lwz r9,0(r31)/lwz r9,12(r9)/bctrl`) is a second, independent
   route, and it is only taken when the splitter returns nonzero **and** the flag byte at
   `sp+8` is set (`0x101a0210`/`0x101a0218` both branch past it otherwise).

The 2.1e silent 200 at span 2139 is consistent with all of this: it touched `r30`'s LSB and
`r31`'s MSB only, and a zeroed MSB is a no-op whenever the stack pointer is already below
`0x01000000`.

### Open unknowns, named (offline work first)

1. **Which length cap binds — this gates everything above.** The handler builds a 4097-byte
   string object (`li r5,4097` at `0x1039c98c`) while the raw-argument descriptor says 1025
   (`0x1039c984`), and the setter is handed the buffer at `sp+16` (the 1025 one). If 1024 binds,
   the share path is truncated around span 1000 and **none** of the saved-register slots are
   reachable; the 2.1e silent 200 does not discriminate, because a NUL at `sp+1017` is also
   silent. The parser does clamp to a descriptor max somewhere (`0x103ae660`–`0x103ae674`:
   `lwz r24,4(r9)` / `cmpw r24,r27` / `ble` / `mr r24,r27`), which is evidence for 1025 binding —
   in which case the saved-register route is **not** reachable through `SetAVTransportURI` and
   the 2.1e NUL is a no-op inside the copy buffer. Settle it before any device run: serve one
   URI of 1100 and one of 2200 chars and watch for a delta, or tie the clamp to the descriptor's
   `+16` max field.
2. **Whether the dispatch is reached** in the 2.1e shape (splitter return + flag at `sp+8`).
   If it is not, only the LR route matters.
3. **Stack ASLR on the device.** A fixed target address is only usable if the stack base is
   deterministic across requests and reboots.

### Closed offline (this round)

- **The percent decoder emits `0x00`.** `sonosDecodePercentEncoded` (`libsonosutils.so.1`,
  `0x51dc`) stores the decoded byte and continues the loop; the terminator is written only at
  the end (`0x5150`). So `%00` is a usable byte in the decoded name buffer — the fake-vtable
  pointer can contain zero bytes.
- **The decoded name buffer is 64 bytes at `sp+0x14d0`, fully attacker-controlled.** The decode
  call is `0x101cbf14`: `r3 = *(sp+0x10b0)` (name source from the splitter `0x10389c24`),
  `r4 = sp+0x14d0`, `r5 = 64`. The vtable pointer read at `sp+0x1500` is name bytes 0x30..0x33,
  inside that buffer.
- **The alignment requirement is arithmetic, not luck.** `fcn.101cbe80` frame 0x2560 →
  `sp ≡ caller_sp − 0x60`; caller `0x101ce2ac` frame 0x20 → `sp ≡ caller_sp − 0x80`; the
  `0x101ce2ac` caller is the AVTransport URI setter `0x101ee058` (frame 0x20) reached from the
  action handler `0x1039c930` (frame 0x1470) through the dispatcher `0x1039c864` (frame 0x1c20).
  `(sp+0x1520) & ~0xFF == sp+0x1500` only when the SOAP-dispatch caller's `sp ≡ 0xA0 (mod 0x100)`.
  One device crash address settles it.
